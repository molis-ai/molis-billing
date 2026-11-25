use serde::Deserialize;
use serde_json::json;
use worker::D1Database;

use super::{
    CreditScope, Delta, LedgerEntry, Result, SYSTEM, UNAVAILABLE, bind, commit_failure, guard,
    new_id, number,
    periods::period_at,
    sync_credit_periods,
    usage::{find_hold, update_period},
};
use crate::{
    http::ApiError,
    idempotency::Audit,
    identity::{Owner, OwnerKind},
    storage::{self, CommitFailure},
};

const CLEANUP_BATCH: u32 = 32;

#[derive(Deserialize)]
struct SubjectRow {
    product: String,
    owner_kind: OwnerKind,
    owner_id: String,
}

#[derive(Deserialize)]
struct PendingOrder {
    id: String,
    product: String,
    owner_kind: OwnerKind,
    owner_id: String,
    payment_expires_at: i64,
    version: i64,
}

/// Work is bounded per invocation. Any remaining due rows are eligible on the next tick.
pub async fn cleanup_due(db: &D1Database, now: i64, request_id: &str) -> Result<()> {
    expire_orders(db, now, request_id).await?;
    let subjects: Vec<SubjectRow> = bind(
        db,
        "SELECT product, owner_kind, owner_id FROM billing_credit_period
           WHERE status='ACTIVE' AND expires_at<=?1
         UNION
         SELECT product, owner_kind, owner_id FROM billing_credit_hold
           WHERE status='OPEN' AND expires_at<=?1
         UNION
         SELECT p.product, p.owner_kind, p.owner_id FROM billing_order_service_period p
           JOIN billing_order o ON o.id=p.order_id
           WHERE p.revoked_at IS NULL AND o.status='PAID' AND p.starts_at<=?1 AND (
             (p.ends_at>?1 AND NOT EXISTS (SELECT 1 FROM billing_credit_period c
                WHERE c.order_id=p.order_id AND c.starts_at<=?1 AND c.expires_at>?1))
             OR (p.ends_at<=?1 AND (SELECT COUNT(*) FROM billing_credit_period c
                WHERE c.order_id=p.order_id)<p.month_count)
           )
         ORDER BY product, owner_kind, owner_id LIMIT ?2",
        &[number(now), CLEANUP_BATCH.into()],
    )?
    .all()
    .await
    .map_err(|_| UNAVAILABLE)?
    .results()
    .map_err(|_| UNAVAILABLE)?;
    for row in subjects {
        let owner = Owner {
            kind: row.owner_kind,
            id: row.owner_id,
        };
        let scope = CreditScope {
            owner: &owner,
            product: &row.product,
        };
        match sync_credit_periods(db, &scope, now, request_id).await {
            Ok(()) | Err(ApiError::VERSION_CONFLICT) => {}
            Err(error) => return Err(error),
        }
    }
    bind(
        db,
        "DELETE FROM billing_idempotency_receipt WHERE rowid IN (
            SELECT rowid FROM billing_idempotency_receipt WHERE expires_at<=?
            ORDER BY expires_at, rowid LIMIT ?
         )",
        &[number(now), CLEANUP_BATCH.into()],
    )?
    .run()
    .await
    .map_err(|_| UNAVAILABLE)?;
    Ok(())
}

async fn expire_orders(db: &D1Database, now: i64, request_id: &str) -> Result<()> {
    let orders: Vec<PendingOrder> = bind(
        db,
        "SELECT id, product, owner_kind, owner_id, payment_expires_at, version
         FROM billing_order WHERE status='PENDING_PAYMENT' AND payment_expires_at<=?
         ORDER BY payment_expires_at, id LIMIT ?",
        &[number(now), CLEANUP_BATCH.into()],
    )?
    .all()
    .await
    .map_err(|_| UNAVAILABLE)?
    .results()
    .map_err(|_| UNAVAILABLE)?;
    for row in orders {
        let owner = Owner {
            kind: row.owner_kind,
            id: row.owner_id,
        };
        let detail = json!({"paymentExpiresAt": row.payment_expires_at});
        let audit = Audit {
            id: &new_id(),
            calling_service: SYSTEM,
            actor_user_id: None,
            operation: "ORDER_EXPIRE",
            target_id: &row.id,
            product: &row.product,
            owner: &owner,
            request_id,
            details: &detail,
            created_at: now,
        };
        let condition = "id=? AND version=? AND status='PENDING_PAYMENT' AND payment_expires_at<=?";
        let values = [row.id.as_str().into(), number(row.version), number(now)];
        let batch = vec![
            guard(
                db,
                &format!("EXISTS (SELECT 1 FROM billing_order WHERE {condition})"),
                &values,
            )?,
            bind(
                db,
                &format!(
                    "UPDATE billing_order SET status='EXPIRED', version=version+1 WHERE {condition}"
                ),
                &values,
            )?,
            audit.prepare(db).map_err(|_| UNAVAILABLE)?,
        ];
        match storage::commit(db, batch).await {
            Ok(_) | Err(CommitFailure::GuardRejected) => {}
            Err(error) => return Err(commit_failure(error)),
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct DueHold {
    id: String,
    occurred_at: i64,
}

pub(super) async fn release_due_holds(
    db: &D1Database,
    scope: &CreditScope<'_>,
    now: i64,
    request_id: &str,
) -> Result<()> {
    let mut values = scope.values().to_vec();
    values.push(number(now));
    let due: Vec<DueHold> = bind(
        db,
        "SELECT h.id, h.occurred_at FROM billing_credit_hold h
         JOIN billing_credit_period p ON p.id=h.credit_period_id
         WHERE h.product=? AND h.owner_kind=? AND h.owner_id=?
           AND h.status='OPEN' AND h.expires_at<=? AND h.expires_at<p.expires_at
         ORDER BY h.expires_at, h.id",
        &values,
    )?
    .all()
    .await
    .map_err(|_| UNAVAILABLE)?
    .results()
    .map_err(|_| UNAVAILABLE)?;
    for row in due {
        for attempt in 0..4 {
            let hold = find_hold(db, scope, &row.id).await?;
            let mut updated = hold.snapshot();
            if updated.status != "OPEN" {
                break;
            }
            let period = period_at(db, scope, row.occurred_at)
                .await?
                .ok_or(UNAVAILABLE)?;
            if period.id != updated.credit_period_id {
                return Err(UNAVAILABLE);
            }
            if period.status == "EXPIRED" {
                // The month closed after the hold read.
                if attempt == 3 {
                    return Err(ApiError::VERSION_CONFLICT);
                }
                continue;
            }
            let amount = updated.reserved;
            updated.reserved = 0;
            updated.released += amount;
            updated.status = "EXPIRED";
            let detail = json!({"reason": "HOLD_TIMEOUT", "amount": amount});
            let audit = Audit {
                id: &new_id(),
                calling_service: SYSTEM,
                actor_user_id: None,
                operation: "CREDIT_HOLD_TIMEOUT",
                target_id: &row.id,
                product: scope.product,
                owner: scope.owner,
                request_id,
                details: &detail,
                created_at: now,
            };
            let delta = Delta {
                available: amount,
                reserved: -amount,
                ..Delta::default()
            };
            let mut batch = hold.update(db, &updated, now)?.to_vec();
            batch.extend(update_period(db, &period, &delta, now)?);
            batch.push(audit.prepare(db).map_err(|_| UNAVAILABLE)?);
            batch.push(
                LedgerEntry {
                    id: &new_id(),
                    period_id: &period.id,
                    hold_id: Some(&row.id),
                    kind: "RELEASE",
                    delta,
                    reference: Some(&updated.reference_id),
                    actor: Some(&hold.actor_user_id),
                    occurred_at: hold.expires_at,
                }
                .prepare(db, &audit)?,
            );
            match storage::commit(db, batch).await {
                Ok(_) => break,
                Err(CommitFailure::GuardRejected) if attempt < 3 => {}
                Err(error) => return Err(commit_failure(error)),
            }
        }
    }
    Ok(())
}
