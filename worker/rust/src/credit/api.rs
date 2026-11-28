use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{D1Database, Request};

use crate::{
    app::{RequestContext, Route},
    core::{
        http::{ApiError, ApiReply, ApiResult},
        identity::{VerifiedUser, require_internal_channel},
        pagination::Page,
        storage::{integer, prepare},
    },
};

use super::{model::CreditScope, mutations, snapshot};

pub(crate) async fn handle_user(
    route: &Route<'_>,
    request: &Request,
    db: &D1Database,
    user: &VerifiedUser,
    ctx: &RequestContext<'_>,
) -> ApiResult<ApiReply> {
    let scope = CreditScope {
        owner: &user.owner,
        product: &user.product,
    };
    let data = match route {
        Route::CreditBalance => {
            let at = ctx.now;
            let period = snapshot::balance(db, &scope, at, ctx.request_id).await?;
            json!({
                "product": scope.product, "ownerKind": scope.owner.kind, "ownerId": scope.owner.id,
                "available": period.as_ref().map_or(0, |row| row.available),
                "granted": period.as_ref().map_or(0, |row| row.granted),
                "reserved": period.as_ref().map_or(0, |row| row.reserved),
                "consumed": period.as_ref().map_or(0, |row| row.consumed),
                "currentCreditPeriod": period.map(|row| json!({
                    "startsAt": row.starts_at, "expiresAt": row.expires_at
                })),
            })
        }
        Route::CreditLedger => {
            let page = Page::from_request(request)?;
            ledger(db, &scope, user.ledger_actor_filter(), page).await?
        }
        _ => return Err(ApiError::NOT_FOUND),
    };
    Ok(ApiReply::ok(data))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct LedgerItem {
    id: String,
    kind: String,
    credit_period_id: String,
    hold_id: Option<String>,
    delta_granted: i64,
    delta_available: i64,
    delta_reserved: i64,
    delta_consumed: i64,
    delta_expired: i64,
    reference_id: Option<String>,
    actor_user_id: Option<String>,
    calling_service: String,
    request_id: String,
    occurred_at: i64,
    created_at: i64,
}

async fn ledger(
    db: &D1Database,
    scope: &CreditScope<'_>,
    actor: Option<&str>,
    page: Page,
) -> ApiResult<Value> {
    let mut sql = String::from(
        "SELECT id, kind, credit_period_id, hold_id, delta_granted, delta_available, delta_reserved,
                delta_consumed, delta_expired, reference_id, actor_user_id, calling_service,
                request_id, occurred_at, created_at
         FROM billing_credit_ledger WHERE product=? AND owner_kind=? AND owner_id=?",
    );
    let mut values = scope.values().to_vec();
    if let Some(actor) = actor {
        sql.push_str(" AND actor_user_id=? AND kind IN ('HOLD','SETTLE','RELEASE','CONSUME')");
        values.push(actor.into());
    }
    if let Some(cursor) = &page.cursor {
        sql.push_str(" AND (created_at,id) < (?,?)");
        values.extend([integer(cursor.created_at)?, cursor.id.as_str().into()]);
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    values.push((page.limit + 1).into());
    let mut items: Vec<LedgerItem> = prepare(db, &sql, &values)?
        .all()
        .await
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
        .results()
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
    let next_cursor = page.paginate_rows(&mut items, |row| (row.created_at, &row.id));
    Ok(json!({ "items": items, "nextCursor": next_cursor }))
}

/// Uses the private Worker entrypoint (or the configured dev loopback) and rejects user credentials.
pub(crate) async fn handle_internal(
    route: &Route<'_>,
    request: Request,
    ctx: &RequestContext<'_>,
) -> ApiResult<ApiReply> {
    require_internal_channel(&request, ctx.env)?;
    if request
        .headers()
        .has("Authorization")
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
        || request
            .headers()
            .has("Cookie")
            .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
    {
        return Err(ApiError::NOT_FOUND);
    }
    mutations::handle(route, request, &ctx.db()?, ctx.now, ctx.request_id).await
}
