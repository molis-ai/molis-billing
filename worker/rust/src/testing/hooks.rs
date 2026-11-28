use worker::{D1Database, D1PreparedStatement, Env, Request};

use crate::{
    app::RequestContext,
    core::{
        calendar::DATE_MAX,
        http::{ApiError, ApiReply, ApiResult},
        identity::require_internal_channel,
        storage::{self, prepare},
    },
    maintenance,
    orders::Orders,
};

#[derive(Clone, Copy, Default)]
pub(crate) struct Hooks {
    enabled: bool,
    pub(crate) now: Option<i64>,
    pub(crate) commit_now: Option<i64>,
    fail_after_grant: bool,
}
impl Hooks {
    pub(crate) fn from_request(request: &Request, env: &Env) -> ApiResult<Self> {
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
                        .filter(|n| (0..=DATE_MAX).contains(n))
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
    pub(crate) fn stage_failed_grant(
        self,
        db: &D1Database,
        batch: &mut Vec<D1PreparedStatement>,
        order_id: &str,
    ) -> ApiResult<()> {
        if self.fail_after_grant {
            batch.push(prepare(
                db,
                "UPDATE billing_credit_ledger SET created_at=created_at
                 WHERE credit_period_id IN (SELECT id FROM billing_credit_period WHERE order_id=?)",
                &[order_id.into()],
            )?);
        }
        Ok(())
    }
}

pub(crate) async fn dispatch(
    path: &str,
    request: &Request,
    ctx: &RequestContext<'_>,
) -> Option<ApiResult<ApiReply>> {
    if !ctx.test.enabled {
        return None;
    }
    Some(match path {
        "/fixture/credits/pay" => match request.clone() {
            Ok(r) => super::credit_fixture::pay(r, ctx).await,
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
                    .expire_pending(ctx.request_id, storage::now() + maintenance::BUDGET_MS)
                    .await?;
                Ok(ApiReply::ok(serde_json::json!({"expired":count})))
            }
            .await
        }
        _ => return None,
    })
}
