use serde::Deserialize;
use serde_json::json;
use worker::D1Database;

use super::{
    CreditScope, Delta, LedgerEntry, Result, SYSTEM, UNAVAILABLE, bind, commit_failure,
    mutations::{find_hold, update_period},
    new_id, number,
    periods::period_at,
    periods::sync_until,
};
use crate::{
    core::http::ApiError,
    core::idempotency::Audit,
    core::identity::{Owner, OwnerKind},
    core::storage::{self, CommitFailure},
};

const CLEANUP_BATCH: u32 = 32;

#[derive(Deserialize)]
struct SubjectRow {
    product: String,
    owner_kind: OwnerKind,
    owner_id: String,
}

pub async fn sync_due(db: &D1Database, now: i64, request_id: &str, deadline: i64) -> Result<()> {
    let subjects: Vec<SubjectRow> = bind(
        db,
        include_str!("sql/cleanup_due.sql"),
        &[number(now)?, CLEANUP_BATCH.into()],
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
        if storage::now() >= deadline {
            break;
        }
        if let Err(error) = sync_until(db, &scope, now, request_id, deadline).await {
            crate::maintenance::log_failure("credits.sync", &owner.id, &error);
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
    deadline: i64,
) -> Result<()> {
    let mut values = scope.values().to_vec();
    values.push(number(now)?);
    let due: Vec<DueHold> = bind(db, include_str!("sql/release_due_holds.sql"), &values)?
        .all()
        .await
        .map_err(|_| UNAVAILABLE)?
        .results()
        .map_err(|_| UNAVAILABLE)?;
    for row in due {
        for attempt in 0..4 {
            if storage::now() >= deadline {
                return Ok(());
            }
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
                operation: "billing.credit.hold.timeout",
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
