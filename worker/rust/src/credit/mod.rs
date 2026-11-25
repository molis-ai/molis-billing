//! Credit reads, synchronization, and trusted usage mutations.
mod cleanup;
#[cfg(feature = "credit-fixture")]
pub(crate) mod fixture;
mod periods;
mod usage;

pub use cleanup::cleanup_due;
pub use periods::{FirstMonthGrant, prepare_paid_first_month, sync_credit_periods};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Env, Request, Response};

use crate::{
    app::Route,
    http::{ApiError, json_response},
    idempotency::Audit,
    identity::{Owner, VerifiedUser, require_internal_channel},
    pagination::{Cursor, Page},
    storage::{self, CommitFailure},
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

fn bind(db: &D1Database, sql: &str, values: &[JsValue]) -> Result<D1PreparedStatement> {
    db.prepare(sql).bind(values).map_err(|_| UNAVAILABLE)
}

fn guard(db: &D1Database, condition: &str, values: &[JsValue]) -> Result<D1PreparedStatement> {
    storage::guard(db, condition, values).map_err(|_| UNAVAILABLE)
}

fn number(value: i64) -> JsValue {
    JsValue::from_f64(value as f64)
}

fn optional(value: Option<&str>) -> JsValue {
    value.map_or(JsValue::NULL, JsValue::from_str)
}

fn now() -> Result<i64> {
    i64::try_from(worker::Date::now().as_millis()).map_err(|_| UNAVAILABLE)
}

fn new_id() -> String {
    crate::http::generated_id()
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
                self.kind.into(), number(self.delta.granted), number(self.delta.available),
                number(self.delta.reserved), number(self.delta.consumed), number(self.delta.expired),
                optional(self.reference), optional(self.actor), audit.calling_service.into(),
                audit.request_id.into(), audit.id.into(), number(self.occurred_at),
                number(audit.created_at),
            ],
        )
    }
}

pub async fn handle_user(
    route: &Route<'_>,
    request: &Request,
    env: &Env,
    user: &VerifiedUser,
    request_id: &str,
) -> worker::Result<Response> {
    render(handle_user_at(route, request, env, user, request_id, now()).await)
}

async fn handle_user_at(
    route: &Route<'_>,
    request: &Request,
    env: &Env,
    user: &VerifiedUser,
    request_id: &str,
    clock: Result<i64>,
) -> Result<(u16, Value)> {
    let db = env.d1("DB").map_err(|_| UNAVAILABLE)?;
    let scope = CreditScope {
        owner: &user.owner,
        product: &user.product,
    };
    let data = match route {
        Route::CreditBalance => {
            let at = clock?;
            sync_credit_periods(&db, &scope, at, request_id).await?;
            let period = periods::period_at(&db, &scope, at).await?;
            json!({
                "product": scope.product, "ownerKind": scope.owner.kind, "ownerId": scope.owner.id,
                "available": period.as_ref().map_or(0, |row| row.available),
                "currentCreditPeriod": period.map(|row| json!({
                    "startsAt": row.starts_at, "expiresAt": row.expires_at
                })),
            })
        }
        Route::CreditLedger => {
            let page = Page::from_request(request)?;
            ledger(&db, &scope, user.ledger_actor_filter(), page).await?
        }
        _ => return Err(ApiError::NOT_FOUND),
    };
    Ok((200, json!({ "data": data })))
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
    if let Some(cursor) = page.cursor {
        sql.push_str(" AND (created_at,id) < (?,?)");
        values.extend([number(cursor.created_at), cursor.id.into()]);
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    values.push((page.limit + 1).into());
    let mut items: Vec<LedgerItem> = bind(db, &sql, &values)?
        .all()
        .await
        .map_err(|_| UNAVAILABLE)?
        .results()
        .map_err(|_| UNAVAILABLE)?;
    let limit = page.limit as usize;
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|item| {
            Cursor {
                created_at: item.created_at,
                id: item.id.clone(),
            }
            .encode()
        })
    } else {
        None
    };
    Ok(json!({ "items": items, "nextCursor": next_cursor }))
}

/// Uses the private Worker entrypoint (or the configured dev loopback) and rejects user credentials.
pub async fn handle_internal(
    route: &Route<'_>,
    request: Request,
    env: &Env,
) -> worker::Result<Response> {
    render(handle_internal_at(route, request, env, now()).await)
}

async fn handle_internal_at(
    route: &Route<'_>,
    request: Request,
    env: &Env,
    clock: Result<i64>,
) -> Result<(u16, Value)> {
    require_internal_channel(&request, env).await?;
    if request
        .headers()
        .has("Authorization")
        .map_err(|_| UNAVAILABLE)?
        || request.headers().has("Cookie").map_err(|_| UNAVAILABLE)?
    {
        return Err(ApiError::NOT_FOUND);
    }
    let db = env.d1("DB").map_err(|_| UNAVAILABLE)?;
    usage::handle(route, request, &db, clock?).await
}

fn render(result: Result<(u16, Value)>) -> worker::Result<Response> {
    match result {
        Ok((status, body)) => json_response(status, &body),
        Err(error) => error.response(),
    }
}
