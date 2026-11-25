#![cfg_attr(not(test), deny(clippy::unwrap_used))]

pub mod app;
pub mod calendar;
pub mod catalog;
pub mod http;
pub mod idempotency;
pub mod identity;
pub mod pagination;
pub mod storage;

use worker::{Context, Env, Request, Response, Result, ScheduleContext, ScheduledEvent, event};

#[event(fetch)]
pub async fn fetch(request: Request, env: Env, _context: Context) -> Result<Response> {
    app::handle(request, env).await
}

#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, _env: Env, _context: ScheduleContext) {
    worker::console_log!("Molis Billing scheduled event received");
}
