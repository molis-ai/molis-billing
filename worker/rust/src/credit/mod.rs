//! Credit reads, synchronization, and trusted usage mutations.
mod cleanup;
#[cfg(feature = "test-fixture")]
pub(crate) mod fixture;
mod mutations;
mod periods;
mod snapshot;

pub use cleanup::sync_due;
pub use periods::{FirstMonthGrant, prepare_paid_first_month, sync_credit_periods};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Request};

use crate::{
    app::{RequestContext, Route},
    core::http::{ApiError, ApiReply, generated_id as new_id},
    core::idempotency::Audit,
    core::identity::{Owner, VerifiedUser, require_internal_channel},
    core::pagination::Page,
    core::storage::{CommitFailure, checked_guard as guard, number, prepare as bind},
};

const UNAVAILABLE: ApiError = ApiError::BILLING_UNAVAILABLE;
const SYSTEM: &str = "billing";

type Result<T> = std::result::Result<T, ApiError>;

pub struct CreditScope<'a> {
    pub owner: &'a Owner,
    pub product: &'a str,
}

impl CreditScope<'_> {
    fn values(&self) -> [JsValue; 3] {
        [self.product, self.owner.kind.as_str(), &self.owner.id].map(JsValue::from_str)
    }
}

fn optional(value: Option<&str>) -> JsValue {
    value.map_or(JsValue::NULL, JsValue::from_str)
}

fn commit_failure(error: CommitFailure) -> ApiError {
    match error {
        CommitFailure::GuardRejected => ApiError::VERSION_CONFLICT,
        CommitFailure::Database => UNAVAILABLE,
    }
}

#[derive(Default)]
struct Delta {
    granted: i64,
    available: i64,
    reserved: i64,
    consumed: i64,
    expired: i64,
}

struct LedgerEntry<'a> {
    id: &'a str,
    period_id: &'a str,
    hold_id: Option<&'a str>,
    kind: &'static str,
    delta: Delta,
    reference: Option<&'a str>,
    actor: Option<&'a str>,
    occurred_at: i64,
}

impl LedgerEntry<'_> {
    fn prepare(&self, db: &D1Database, audit: &Audit<'_>) -> Result<D1PreparedStatement> {
        bind(
            db,
            "INSERT INTO billing_credit_ledger
             (id, credit_period_id, hold_id, product, owner_kind, owner_id, kind,
              delta_granted, delta_available, delta_reserved, delta_consumed, delta_expired,
              reference_id, actor_user_id, calling_service, request_id, audit_id, occurred_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                self.id.into(), self.period_id.into(), optional(self.hold_id),
                audit.product.into(), audit.owner.kind.as_str().into(), audit.owner.id.as_str().into(),
                self.kind.into(), number(self.delta.granted)?, number(self.delta.available)?,
                number(self.delta.reserved)?, number(self.delta.consumed)?, number(self.delta.expired)?,
                optional(self.reference), optional(self.actor), audit.calling_service.into(),
                audit.request_id.into(), audit.id.into(), number(self.occurred_at)?,
                number(audit.created_at)?,
            ],
        )
    }
}

pub async fn handle_user(
    route: &Route<'_>,
    request: &Request,
    db: &D1Database,
    user: &VerifiedUser,
    ctx: &RequestContext<'_>,
) -> Result<ApiReply> {
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
) -> Result<Value> {
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
        values.extend([number(cursor.created_at)?, cursor.id.as_str().into()]);
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    values.push((page.limit + 1).into());
    let mut items: Vec<LedgerItem> = bind(db, &sql, &values)?
        .all()
        .await
        .map_err(|_| UNAVAILABLE)?
        .results()
        .map_err(|_| UNAVAILABLE)?;
    let next_cursor = page.paginate_rows(&mut items, |row| (row.created_at, &row.id));
    Ok(json!({ "items": items, "nextCursor": next_cursor }))
}

/// Uses the private Worker entrypoint (or the configured dev loopback) and rejects user credentials.
pub async fn handle_internal(
    route: &Route<'_>,
    request: Request,
    ctx: &RequestContext<'_>,
) -> Result<ApiReply> {
    require_internal_channel(&request, ctx.env)?;
    if request
        .headers()
        .has("Authorization")
        .map_err(|_| UNAVAILABLE)?
        || request.headers().has("Cookie").map_err(|_| UNAVAILABLE)?
    {
        return Err(ApiError::NOT_FOUND);
    }
    let (status, body) =
        mutations::handle(route, request, &ctx.db()?, ctx.now, ctx.request_id).await?;
    Ok(ApiReply { status, body })
}
