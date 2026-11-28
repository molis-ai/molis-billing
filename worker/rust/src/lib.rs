#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::wildcard_imports))]

mod app;
mod audit;
mod catalog;
mod core;
mod credit;
mod maintenance;
mod orders;
#[cfg(feature = "test-fixture")]
mod testing;
use worker::{Context, Env, Request, Response, Result, ScheduleContext, ScheduledEvent, event};
#[event(fetch)]
pub async fn fetch(request: Request, env: Env, _context: Context) -> Result<Response> {
    app::handle(request, env).await
}

#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, _context: ScheduleContext) {
    maintenance::run(&env).await;
}
