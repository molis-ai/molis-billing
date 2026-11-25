#![cfg_attr(not(test), deny(clippy::unwrap_used))]

pub mod app;
pub mod calendar;
pub mod catalog;
pub mod http;
pub mod idempotency;
pub mod identity;
pub mod orders;
pub mod pagination;
pub mod storage;

pub mod credit;

use worker::{Context, Env, Request, Response, Result, ScheduleContext, ScheduledEvent, event};

#[event(fetch)]
pub async fn fetch(request: Request, env: Env, _context: Context) -> Result<Response> {
    #[cfg(feature = "credit-fixture")]
    if credit::fixture::enabled(&env) {
        return credit::fixture::handle(request, env).await;
    }
    app::handle(request, env).await
}

#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, _context: ScheduleContext) {
    if let Err(error) = run_credit_cleanup(&env).await {
        worker::console_error!("Billing scheduled cleanup failed: {}", error.code);
    }
}

async fn run_credit_cleanup(env: &Env) -> std::result::Result<(), http::ApiError> {
    let db = env
        .d1("DB")
        .map_err(|_| http::ApiError::BILLING_UNAVAILABLE)?;
    let now = i64::try_from(worker::Date::now().as_millis())
        .map_err(|_| http::ApiError::BILLING_UNAVAILABLE)?;
    credit::cleanup_due(&db, now, &http::generated_id()).await
}
