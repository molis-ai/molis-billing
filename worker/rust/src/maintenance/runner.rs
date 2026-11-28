use serde::Serialize;
use serde_json::json;
use worker::{D1Database, Env};

use crate::{
    core::{
        http::{ApiError, ApiResult, generated_id},
        idempotency, logging, storage,
    },
    credit,
    orders::Orders,
};

pub(crate) const CLEANUP_BATCH: u32 = 32;
pub(crate) const BUDGET_MS: i64 = 8_000;

#[derive(Default)]
pub(crate) struct BatchResult {
    pub(crate) processed: usize,
    pub(crate) failed: usize,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Step {
    Complete { processed: usize, failed: usize },
    Failed { code: &'static str },
    Skipped,
}

impl Step {
    fn has_failure(&self) -> bool {
        matches!(
            self,
            Self::Failed { .. } | Self::Complete { failed: 1.., .. }
        )
    }
}

pub(crate) struct MaintenanceLog<'a> {
    env: &'static str,
    pub(crate) request_id: &'a str,
}

impl<'a> MaintenanceLog<'a> {
    pub(crate) fn new(env: &Env, request_id: &'a str) -> Self {
        Self {
            env: logging::environment(env),
            request_id,
        }
    }

    pub(crate) fn failure(&self, task: &'static str, error: &ApiError) {
        logging::event(
            "billing.maintenance.failure",
            self.env,
            self.request_id,
            [("task", json!(task)), ("code", json!(error.code))],
            true,
        );
    }

    fn step(&self, task: &'static str, result: ApiResult<BatchResult>) -> Step {
        match result {
            Ok(result) => Step::Complete {
                processed: result.processed,
                failed: result.failed,
            },
            Err(error) => {
                self.failure(task, &error);
                Step::Failed { code: error.code }
            }
        }
    }

    fn finish(
        &self,
        started: i64,
        database_available: bool,
        expire: Step,
        sync: Step,
        purge: Step,
    ) {
        let duration = (storage::now() - started).max(0);
        let failed = !database_available
            || expire.has_failure()
            || sync.has_failure()
            || purge.has_failure();
        logging::event(
            "billing.maintenance",
            self.env,
            self.request_id,
            [
                ("duration_ms", json!(duration)),
                ("budget_exhausted", json!(duration >= BUDGET_MS)),
                ("failed", json!(failed)),
                ("expire", json!(expire)),
                ("sync", json!(sync)),
                ("purge", json!(purge)),
            ],
            failed,
        );
    }
}

pub(crate) async fn run(env: &Env) {
    let started = storage::now();
    let id = generated_id();
    let log = MaintenanceLog::new(env, &id);
    let Ok(db) = env.d1("DB") else {
        log.failure("maintenance", &ApiError::BILLING_UNAVAILABLE);
        log.finish(started, false, Step::Skipped, Step::Skipped, Step::Skipped);
        return;
    };
    run_at(&db, started, &log).await;
}

pub(crate) async fn run_at(db: &D1Database, now: i64, log: &MaintenanceLog<'_>) {
    let started = storage::now();
    let deadline = started + BUDGET_MS;
    let expire = log.step(
        "orders.expire",
        Orders::for_maintenance(db, now)
            .expire_pending(log, deadline)
            .await,
    );
    let sync = if storage::now() < deadline {
        log.step(
            "credits.sync",
            credit::sync_due(db, now, log, deadline).await,
        )
    } else {
        Step::Skipped
    };
    let purge = if storage::now() < deadline {
        let result = idempotency::purge_expired(db, now, CLEANUP_BATCH).await;
        log.step(
            "receipts.purge",
            result.map(|processed| BatchResult {
                processed,
                failed: 0,
            }),
        )
    } else {
        Step::Skipped
    };
    log.finish(started, true, expire, sync, purge);
}
