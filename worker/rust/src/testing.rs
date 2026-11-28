use crate::{
    app::RequestContext,
    core::{
        http::{ApiError, ApiReply},
        identity::require_internal_channel,
        storage::{self, ApiResult},
    },
    credit, maintenance,
    orders::Orders,
};
use worker::{Env, Request};

#[derive(Clone, Copy, Default)]
pub struct Hooks {
    enabled: bool,
    pub now: Option<i64>,
    pub commit_now: Option<i64>,
    pub fail_after_grant: bool,
}
impl Hooks {
    pub fn from_request(request: &Request, env: &Env) -> ApiResult<Self> {
        if !env
            .var("ENVIRONMENT_ID")
            .is_ok_and(|e| e.to_string() == "billing-isolated-test")
        {
            return Ok(Self::default());
        }
        let time = |header| -> ApiResult<Option<i64>> {
            request
                .headers()
                .get(header)
                .map_err(|_| ApiError::INVALID_REQUEST)?
                .map(|v| {
                    v.parse::<i64>()
                        .ok()
                        .filter(|n| (0..=8_640_000_000_000_000).contains(n))
                        .ok_or(ApiError::INVALID_REQUEST)
                })
                .transpose()
        };
        Ok(Self {
            enabled: true,
            now: time("X-Test-Now")?,
            commit_now: time("X-Test-Commit-Now")?,
            fail_after_grant: request
                .headers()
                .get("X-Test-Fail-After-Grant")
                .ok()
                .flatten()
                .as_deref()
                == Some("1"),
        })
    }
}

pub async fn dispatch(
    path: &str,
    request: &Request,
    ctx: &RequestContext<'_>,
) -> Option<ApiResult<ApiReply>> {
    if !ctx.test.enabled {
        return None;
    }
    Some(match path {
        "/fixture/credits/pay" => match request.clone() {
            Ok(r) => credit::fixture::pay(r, ctx).await,
            Err(_) => Err(ApiError::INVALID_REQUEST),
        },
        "/fixture/credits/cleanup" => {
            async {
                require_internal_channel(request, ctx.env)?;
                maintenance::run_at(&ctx.db()?, ctx.now, ctx.request_id).await;
                Ok(ApiReply::ok(serde_json::json!({})))
            }
            .await
        }
        "/fixture/orders/expire" => {
            async {
                require_internal_channel(request, ctx.env)?;
                let db = ctx.db()?;
                let count = Orders::for_maintenance(&db, ctx.now)
                    .expire_pending(ctx.request_id, storage::now() + 8000)
                    .await?;
                Ok(ApiReply::ok(serde_json::json!({"expired":count})))
            }
            .await
        }
        _ => return None,
    })
}
