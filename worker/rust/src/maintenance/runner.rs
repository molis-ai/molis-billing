use worker::{D1Database, Env};

use crate::{
    core::{
        http::{ApiError, generated_id},
        idempotency, storage,
    },
    credit,
    orders::Orders,
};

pub(crate) const CLEANUP_BATCH: u32 = 32;
pub(crate) const BUDGET_MS: i64 = 8_000;

pub(crate) fn log_failure(task: &str, target: &str, error: &ApiError) {
    worker::console_error!(
        "{}",
        serde_json::json!({"task":task,"targetId":target,"errorCode":error.code})
    );
}

pub(crate) async fn run(env: &Env) {
    let Ok(db) = env.d1("DB") else {
        log_failure("maintenance", "DB", &ApiError::BILLING_UNAVAILABLE);
        return;
    };
    run_at(&db, storage::now(), &generated_id()).await;
}

pub(crate) async fn run_at(db: &D1Database, now: i64, id: &str) {
    let deadline = storage::now() + BUDGET_MS;
    if let Err(error) = Orders::for_maintenance(db, now)
        .expire_pending(id, deadline)
        .await
    {
        log_failure("orders.expire", id, &error);
    }
    if storage::now() < deadline
        && let Err(error) = credit::sync_due(db, now, id, deadline).await
    {
        log_failure("credits.sync", id, &error);
    }
    if storage::now() < deadline
        && let Err(error) = idempotency::purge_expired(db, now, CLEANUP_BATCH).await
    {
        log_failure("receipts.purge", id, &error);
    }
}
