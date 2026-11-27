//! Order reads, writes, and the atomic payment integration point.
mod model;
mod payments;

#[cfg(feature = "order-fixture")]
pub(crate) mod fixture;

use js_sys::Date;
use serde::Deserialize;
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;
use worker::{D1Database, D1PreparedStatement, Env, Request, Response};

use crate::{
    app::Route,
    calendar::{self, Anchor, Interval, Period},
    catalog,
    credit::{CreditScope, FirstMonthGrant, prepare_paid_first_month},
    http::{ApiError, json_response, request_id, valid_id},
    idempotency::{self, Audit, Receipt, ReceiptKey, Success},
    identity::{Owner, VerifiedUser},
    pagination::{Cursor, Page},
    storage::{self, CommitFailure, integer},
};
use model::{Order, Subject, Timeline};
pub use payments::webhook;

type ApiResult<T> = Result<T, ApiError>;
const PAYMENT_WINDOW_MS: i64 = 1_800_000;
const RECEIPT_TTL_MS: i64 = 86_400_000;
const MAX_ATTEMPTS: usize = 3;
const DB_CLOCK: &str = "CAST(ROUND(unixepoch('subsec') * 1000) AS INTEGER)";

/// The Credit node appends the initial grant and ledger statements here.
/// All order, payment event, audit, Credit, and receipt statements commit together.
pub type AppendFirstCredit =
    fn(&D1Database, &FirstCredit<'_>, &mut Vec<D1PreparedStatement>) -> ApiResult<()>;

pub struct FirstCredit<'a> {
    pub order_id: &'a str,
    pub product: &'a str,
    pub owner: &'a Owner,
    pub anchor: &'a Anchor,
    pub period: Period,
    pub monthly_credits: i64,
    pub now: i64,
    pub audit_id: &'a str,
    pub request_id: &'a str,
}

pub fn append_first_credit(
    db: &D1Database,
    first: &FirstCredit<'_>,
    batch: &mut Vec<D1PreparedStatement>,
) -> ApiResult<()> {
    batch.extend(prepare_paid_first_month(
        db,
        &CreditScope {
            owner: first.owner,
            product: first.product,
        },
        FirstMonthGrant {
            order_id: first.order_id,
            anchor: first.anchor,
            period: &first.period,
            monthly_credits: first.monthly_credits,
            paid_at: first.now,
            request_id: first.request_id,
        },
    )?);
    Ok(())
}

#[derive(Clone, Copy)]
pub struct Orders<'a> {
    db: &'a D1Database,
    append_first_credit: AppendFirstCredit,
    #[cfg(feature = "order-fixture")]
    fixed_time: Option<i64>,
    #[cfg(feature = "order-fixture")]
    fixed_commit_time: Option<i64>,
}

impl<'a> Orders<'a> {
    pub fn new(db: &'a D1Database, append_first_credit: AppendFirstCredit) -> Self {
        Self {
            db,
            append_first_credit,
            #[cfg(feature = "order-fixture")]
            fixed_time: None,
            #[cfg(feature = "order-fixture")]
            fixed_commit_time: None,
        }
    }

    /// Call only with a fresh, route-appropriate Auth verification.
    /// None allows the main router to dispatch routes owned by other modules.
    pub async fn dispatch(
        self,
        route: &Route<'_>,
        mut request: Request,
        env: &Env,
        user: &VerifiedUser,
        request_id: &str,
    ) -> Option<worker::Result<Response>> {
        let subject = Subject {
            product: &user.product,
            owner: &user.owner,
        };
        let result = match route {
            Route::Subscription => self.subscription(subject, user).await.map(Reply::ok),
            Route::Entitlement => self.entitlement(subject).await.map(Reply::ok),
            Route::ListOrders => self.list(subject, &request).await.map(Reply::ok),
            Route::GetOrder(id) => self.get(subject, id).await.map(Reply::ok),
            Route::CreateOrder => self.create(subject, user, &mut request, request_id).await,
            #[cfg(feature = "mock-payment")]
            Route::MockPay(id) => {
                self.confirm(user, &request, env, id, request_id, false)
                    .await
            }
            Route::Refund(id) => {
                self.confirm(user, &request, env, id, request_id, true)
                    .await
            }
            _ => return None,
        };
        Some(respond(result))
    }

    fn now(self) -> i64 {
        #[cfg(feature = "order-fixture")]
        if let Some(now) = self.fixed_time {
            return now;
        }
        Date::now() as i64
    }

    /// The first bind is the processing timestamp. The D1 clock rechecks that
    /// the same business time window is still open when the write commits.
    fn time_guard(self, predicate: &str, params: &[JsValue]) -> ApiResult<D1PreparedStatement> {
        let clock = DB_CLOCK.to_owned();
        #[cfg(feature = "order-fixture")]
        let clock = self
            .fixed_commit_time
            .or(self.fixed_time)
            .map_or(clock, |time| time.to_string());
        db_result(storage::guard(
            self.db,
            &format!(
                "WITH clock(now) AS (SELECT MAX(?1, {clock})) SELECT ({predicate}) FROM clock"
            ),
            params,
        ))
    }

    async fn now_after_guard_miss(self, previous: i64) -> ApiResult<i64> {
        #[cfg(feature = "order-fixture")]
        if let Some(now) = self.fixed_commit_time.or(self.fixed_time) {
            return Ok(previous.max(now));
        }
        let db_now: i64 = db_result(
            self.db
                .prepare(format!("SELECT {DB_CLOCK} AS now"))
                .first(Some("now"))
                .await,
        )?
        .ok_or(ApiError::BILLING_UNAVAILABLE)?;
        Ok(previous.max(db_now))
    }

    async fn get(self, subject: Subject<'_>, id: &str) -> ApiResult<Value> {
        let order = Order::find(self.db, subject, id).await?;
        Ok(order.data(self.now()))
    }

    async fn list(self, subject: Subject<'_>, request: &Request) -> ApiResult<Value> {
        let page = Page::from_request(request)?;
        let mut rows = Order::list(self.db, subject, &page).await?;
        let next = if rows.len() > page.limit as usize {
            rows.pop();
            rows.last().map(|row| {
                Cursor {
                    id: row.id.clone(),
                    created_at: row.created_at,
                }
                .encode()
            })
        } else {
            None
        };
        let now = self.now();
        let items: Vec<_> = rows.iter().map(|row| row.data(now)).collect();
        Ok(json!({"items": items, "nextCursor": next}))
    }

    async fn subscription(self, subject: Subject<'_>, user: &VerifiedUser) -> ApiResult<Value> {
        let timeline = Timeline::load(self.db, subject, self.now()).await?;
        let visible = user.ledger_actor_filter().is_none();
        let paid_through = timeline.future.as_ref().or(timeline.current.as_ref());
        Ok(json!({
            "product": subject.product,
            "ownerKind": subject.owner.kind,
            "ownerId": subject.owner.id,
            "billingTimeZone": paid_through.map(|p| &p.billing_time_zone),
            "currentPeriod": timeline.current.as_ref().map(|p| p.data(visible)),
            "futurePeriod": timeline.future.as_ref().map(|p| p.data(visible)),
            "paidThroughAt": paid_through.map(|p| p.ends_at),
        }))
    }

    async fn entitlement(self, subject: Subject<'_>) -> ApiResult<Value> {
        let current = Timeline::load(self.db, subject, self.now()).await?.current;
        Ok(json!({
            "active": current.is_some(),
            "planId": current.as_ref().map(|period| &period.plan_id),
            "endsAt": current.as_ref().map(|period| period.ends_at),
        }))
    }

    async fn create(
        self,
        subject: Subject<'_>,
        user: &VerifiedUser,
        request: &mut Request,
        request_id: &str,
    ) -> ApiResult<Reply> {
        let key = idempotency::header(request)?;
        let body: CreateOrder = crate::http::read_json(request)
            .await?
            .ok_or(ApiError::INVALID_REQUEST)?;
        if !valid_id(&body.plan_id) {
            return Err(ApiError::INVALID_REQUEST);
        }
        let zone = calendar::validate_time_zone(body.billing_time_zone.as_deref())?;
        let write = UserWrite::new(
            subject,
            user,
            "order.create",
            &key,
            &body.plan_id,
            &json!({"interval": body.interval, "billingTimeZone": zone}),
        )
        .await?;
        for _ in 0..MAX_ATTEMPTS {
            if let Some(reply) = write.replay(self.db, self.now()).await? {
                return Ok(reply);
            }
            let quote =
                catalog::find_quote(self.db, subject.product, subject.owner.kind, &body.plan_id)
                    .await?;
            let now = self.now();
            let timeline = Timeline::load(self.db, subject, now).await?;
            timeline.require_plan(&body.plan_id)?;
            let order = Order::pending(subject, user, &quote, body.interval, &zone, now);
            let reply = Reply::new(201, order.data(now));
            let mut values = vec![number(now)?];
            values.extend(subject.bind());
            values.extend([order.plan_id.as_str().into(), number(order.plan_version)?]);
            let audit_id = new_id();
            let mut batch = vec![
                self.time_guard(
                    "EXISTS (SELECT 1 FROM billing_plan
                       WHERE product=?2 AND owner_kind=?3 AND plan_id=?5
                         AND current_version=?6 AND sale_status='ON_SALE')
                     AND NOT EXISTS (SELECT 1 FROM billing_order_service_period
                       WHERE product=?2 AND owner_kind=?3 AND owner_id=?4 AND revoked_at IS NULL
                         AND ends_at > now AND plan_id != ?5)",
                    &values,
                )?,
                order.insert(self.db)?,
                audit(
                    self.db,
                    &audit_id,
                    &write.key,
                    &order.id,
                    request_id,
                    &json!({"planId": order.plan_id, "planVersion": order.plan_version}),
                    now,
                )?,
            ];
            write.append(self.db, &mut batch, &audit_id, &reply, now)?;
            match storage::commit(self.db, batch).await {
                Ok(_) => return Ok(reply),
                Err(CommitFailure::GuardRejected) => continue,
                Err(CommitFailure::Database) => {
                    return write.recover(self.db, self.now()).await;
                }
            }
        }
        if let Some(reply) = write.replay(self.db, self.now()).await? {
            return Ok(reply);
        }
        catalog::find_quote(self.db, subject.product, subject.owner.kind, &body.plan_id).await?;
        Err(ApiError::VERSION_CONFLICT)
    }

    /// Persist expired orders in bounded batches. Reads enforce the deadline
    /// even if the scheduled caller hasn't swept the pending row yet.
    pub async fn expire_pending(self, request_id: &str, limit: u32) -> ApiResult<u32> {
        let rows = Order::pending_expired(self.db, self.now(), limit.clamp(1, 100)).await?;
        let mut expired = 0;
        for order in rows {
            if self.expire(&order, request_id, self.now()).await? {
                expired += 1;
            }
        }
        Ok(expired)
    }

    async fn expire(self, order: &Order, request_id: &str, now: i64) -> ApiResult<bool> {
        let owner = order.owner();
        let audit_id = new_id();
        let system = ReceiptKey {
            calling_service: "billing",
            actor_user_id: "",
            operation: "order.expire",
            product: &order.product,
            owner: &owner,
            key: "",
        };
        let values = [
            order.id.as_str().into(),
            number(order.version)?,
            number(now)?,
        ];
        let batch = vec![
            db_result(storage::guard(
                self.db,
                "EXISTS (SELECT 1 FROM billing_order
                   WHERE id=? AND version=? AND status='PENDING_PAYMENT' AND payment_expires_at<=?)",
                &values,
            ))?,
            prepare(
                self.db,
                "UPDATE billing_order SET status='EXPIRED', version=version+1 WHERE id=?",
                &[order.id.as_str().into()],
            )?,
            audit(self.db, &audit_id, &system, &order.id, request_id, &json!({}), now)?,
        ];
        match storage::commit(self.db, batch).await {
            Ok(_) => Ok(true),
            Err(CommitFailure::GuardRejected) => Ok(false),
            Err(CommitFailure::Database) => Err(ApiError::BILLING_UNAVAILABLE),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateOrder {
    plan_id: String,
    interval: Interval,
    billing_time_zone: Option<String>,
}

struct Reply {
    status: u16,
    body: Value,
}

impl Reply {
    fn new(status: u16, data: Value) -> Self {
        Self {
            status,
            body: json!({"data": data}),
        }
    }

    fn ok(data: Value) -> Self {
        Self::new(200, data)
    }

    fn stored(status: u16, body: &str) -> ApiResult<Self> {
        let body = serde_json::from_str(body).map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
        Ok(Self { status, body })
    }
}

fn respond(reply: ApiResult<Reply>) -> worker::Result<Response> {
    match reply {
        Ok(reply) => json_response(reply.status, &reply.body),
        Err(error) => error.response(),
    }
}

struct UserWrite<'a> {
    key: ReceiptKey<'a>,
    hash: String,
}

impl<'a> UserWrite<'a> {
    async fn new(
        subject: Subject<'a>,
        user: &'a VerifiedUser,
        operation: &'a str,
        key: &'a str,
        target: &str,
        parameters: &Value,
    ) -> ApiResult<Self> {
        let key = ReceiptKey {
            calling_service: "auth",
            actor_user_id: &user.user_id,
            operation,
            product: subject.product,
            owner: subject.owner,
            key,
        };
        let hash = db_result(key.request_hash(target, parameters).await)?;
        Ok(Self { key, hash })
    }

    async fn replay(&self, db: &D1Database, now: i64) -> ApiResult<Option<Reply>> {
        self.key
            .lookup(db, now, &self.hash)
            .await?
            .map(
                |Receipt {
                     response_status,
                     response_json,
                     ..
                 }| { Reply::stored(response_status, &response_json) },
            )
            .transpose()
    }

    async fn recover(&self, db: &D1Database, now: i64) -> ApiResult<Reply> {
        self.replay(db, now)
            .await?
            .ok_or(ApiError::BILLING_UNAVAILABLE)
    }

    fn append(
        &self,
        db: &D1Database,
        batch: &mut Vec<D1PreparedStatement>,
        audit_id: &str,
        reply: &Reply,
        now: i64,
    ) -> ApiResult<()> {
        batch.extend(db_result(self.key.success(
            db,
            Success {
                request_hash: &self.hash,
                response_status: reply.status,
                response: &reply.body,
                audit_id,
                created_at: now,
                expires_at: now + RECEIPT_TTL_MS,
            },
        ))?);
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn audit(
    db: &D1Database,
    id: &str,
    scope: &ReceiptKey<'_>,
    target_id: &str,
    request_id: &str,
    details: &Value,
    now: i64,
) -> ApiResult<D1PreparedStatement> {
    db_result(
        Audit {
            id,
            calling_service: scope.calling_service,
            actor_user_id: (!scope.actor_user_id.is_empty()).then_some(scope.actor_user_id),
            operation: scope.operation,
            target_id,
            product: scope.product,
            owner: scope.owner,
            request_id,
            details,
            created_at: now,
        }
        .prepare(db),
    )
}

fn new_id() -> String {
    crate::http::generated_id()
}

fn prepare(db: &D1Database, sql: &str, values: &[JsValue]) -> ApiResult<D1PreparedStatement> {
    db_result(db.prepare(sql).bind(values))
}

fn db_result<T>(result: worker::Result<T>) -> ApiResult<T> {
    result.map_err(|_| ApiError::BILLING_UNAVAILABLE)
}

fn number(value: i64) -> ApiResult<JsValue> {
    db_result(integer(value))
}
