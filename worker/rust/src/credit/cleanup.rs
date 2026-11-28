use serde::Deserialize;
use serde_json::json;
use worker::D1Database;

use crate::{
    core::{
        http::{ApiError, ApiResult, generated_id as new_id},
        idempotency::Audit,
        identity::{Owner, OwnerKind},
        storage::{self, CommitFailure, integer, prepare},
    },
    maintenance::{BatchResult, CLEANUP_BATCH, MaintenanceLog},
};

use super::{
    model::{CreditScope, Delta, LedgerEntry, SYSTEM},
    mutations::{find_hold, update_period},
    periods::{period_at, sync_until},
};

#[derive(Deserialize)]
struct SubjectRow {
    product: String,
    owner_kind: OwnerKind,
    owner_id: String,
}

pub(crate) async fn sync_due(
    db: &D1Database,
    now: i64,
    log: &MaintenanceLog<'_>,
    deadline: i64,
) -> ApiResult<BatchResult> {
    let subjects: Vec<SubjectRow> = prepare(
        db,
        include_str!("sql/cleanup_due.sql"),
        &[integer(now)?, CLEANUP_BATCH.into()],
    )?
    .all()
    .await
    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
    .results()
    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
    let mut result = BatchResult::default();
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
        match sync_until(db, &scope, now, log.request_id, deadline).await {
            Ok(()) => result.processed += 1,
            Err(error) => {
                result.failed += 1;
                log.failure("credits.sync", &error);
            }
        }
    }
    Ok(result)
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
) -> ApiResult<()> {
    let mut values = scope.values().to_vec();
    values.push(integer(now)?);
    let due: Vec<DueHold> = prepare(db, include_str!("sql/release_due_holds.sql"), &values)?
        .all()
        .await
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
        .results()
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
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
                .ok_or(ApiError::BILLING_UNAVAILABLE)?;
            if period.id != updated.credit_period_id {
                return Err(ApiError::BILLING_UNAVAILABLE);
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
            batch.push(audit.prepare(db)?);
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
                Err(error) => return Err(error.into_api_error()),
            }
        }
    }
    Ok(())
}
