use serde::Deserialize;
use serde_json::{Value, json};
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Env, Request};

use crate::{
    app::{RequestContext, Route},
    catalog,
    core::calendar::{self, Interval},
    core::http::{ApiError, ApiReply, ApiResult, generated_id as new_id, valid_id},
    core::idempotency::{self, Audit, RECEIPT_TTL_MS, Receipt, ReceiptKey, Success},
    core::identity::VerifiedUser,
    core::pagination::Page,
    core::storage::{self, CommitFailure, db_result, integer, prepare},
    maintenance::{BatchResult, CLEANUP_BATCH, MaintenanceLog},
};

use super::model::{Order, Subject, Timeline};

pub(super) const PAYMENT_WINDOW_MS: i64 = 1_800_000;
pub(super) const MAX_ATTEMPTS: usize = 3;
const DB_CLOCK: &str = "CAST(ROUND(unixepoch('subsec') * 1000) AS INTEGER)";

#[derive(Clone, Copy)]
pub(crate) struct Orders<'a> {
    pub(super) db: &'a D1Database,
    pub(super) now: i64,
    #[cfg(feature = "test-fixture")]
    pub(super) test: crate::testing::Hooks,
}

impl<'a> Orders<'a> {
    pub(crate) fn new(db: &'a D1Database, context: &RequestContext<'_>) -> Self {
        Self {
            db,
            now: context.now,
            #[cfg(feature = "test-fixture")]
            test: context.test,
        }
    }

    pub(crate) fn for_maintenance(db: &'a D1Database, now: i64) -> Self {
        Self {
            db,
            now,
            #[cfg(feature = "test-fixture")]
            test: Default::default(),
        }
    }

    /// Call only with a fresh, route-appropriate Auth verification.
    pub(crate) async fn dispatch(
        self,
        route: &Route<'_>,
        mut request: Request,
        env: &Env,
        user: &VerifiedUser,
        request_id: &str,
    ) -> ApiResult<ApiReply> {
        let subject = Subject {
            product: &user.product,
            owner: &user.owner,
        };
        match route {
            Route::Subscription => self.subscription(subject, user).await.map(ApiReply::ok),
            Route::Entitlement => self.entitlement(subject).await.map(ApiReply::ok),
            Route::ListOrders => self.list(subject, &request).await.map(ApiReply::ok),
            Route::GetOrder(id) => self.get(subject, id).await.map(ApiReply::ok),
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
            _ => Err(ApiError::NOT_FOUND),
        }
    }

    pub(super) async fn retry_time(self, previous: i64) -> ApiResult<i64> {
        #[cfg(feature = "test-fixture")]
        if let Some(now) = self.test.commit_now.or(self.test.now) {
            return Ok(previous.max(now));
        }
        // On a rejected write, classify the boundary using the same clock as its SQL guard.
        let now = db_result(
            self.db
                .prepare(format!("SELECT {DB_CLOCK} AS now"))
                .first::<i64>(Some("now"))
                .await,
        )?
        .ok_or(ApiError::BILLING_UNAVAILABLE)?;
        Ok(previous.max(now))
    }

    /// The first bind is the processing timestamp. The D1 clock rechecks that
    /// the same business time window is still open when the write commits.
    pub(super) fn time_guard(
        self,
        predicate: &str,
        params: &[JsValue],
    ) -> ApiResult<D1PreparedStatement> {
        let clock = DB_CLOCK.to_owned();
        #[cfg(feature = "test-fixture")]
        let clock = self
            .test
            .commit_now
            .or(self.test.now)
            .map_or(clock, |time| time.to_string());
        storage::guard(
            self.db,
            &format!(
                "WITH clock(now) AS (SELECT MAX(?1, {clock})) SELECT ({predicate}) FROM clock"
            ),
            params,
        )
    }

    async fn get(self, subject: Subject<'_>, id: &str) -> ApiResult<Value> {
        let order = Order::find(self.db, subject, id).await?;
        Ok(order.data(self.now))
    }

    async fn list(self, subject: Subject<'_>, request: &Request) -> ApiResult<Value> {
        let page = Page::from_request(request)?;
        let mut rows = Order::list(self.db, subject, &page).await?;
        let next = page.paginate_rows(&mut rows, |row| (row.created_at, &row.id));
        let now = self.now;
        let items: Vec<_> = rows.iter().map(|row| row.data(now)).collect();
        Ok(json!({"items": items, "nextCursor": next}))
    }

    async fn subscription(self, subject: Subject<'_>, user: &VerifiedUser) -> ApiResult<Value> {
        let timeline = Timeline::load(self.db, subject, self.now).await?;
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
        let current = Timeline::load(self.db, subject, self.now).await?.current;
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
    ) -> ApiResult<ApiReply> {
        let key = idempotency::header(request)?;
        let body: CreateOrder = crate::core::http::read_json(request)
            .await?
            .ok_or(ApiError::INVALID_REQUEST)?;
        if !valid_id(&body.plan_id) {
            return Err(ApiError::INVALID_REQUEST);
        }
        let zone = calendar::validate_time_zone(body.billing_time_zone.as_deref())?;
        let write = UserWrite::new(
            subject,
            user,
            "billing.order.create",
            &key,
            &body.plan_id,
            &json!({"interval": body.interval, "billingTimeZone": zone}),
        );
        let mut now = self.now;
        for _ in 0..MAX_ATTEMPTS {
            let [receipt, quote, timeline] = storage::batch_read(
                self.db,
                [
                    write.key.read(self.db, now)?,
                    catalog::read_quote(
                        self.db,
                        subject.product,
                        subject.owner.kind,
                        &body.plan_id,
                    )?,
                    Timeline::read(self.db, &subject.bind(), now)?,
                ],
            )
            .await?;
            if let Some(reply) = write.replay_receipt(storage::first_row(&receipt)?)? {
                return Ok(reply);
            }
            let quote = catalog::require_quote(storage::first_row(&quote)?)?;
            let timeline = Timeline::from_rows(storage::rows(&timeline)?, now)?;
            timeline.require_plan(&body.plan_id)?;
            let order = Order::pending(subject, user, &quote, body.interval, &zone, now);
            let reply = ApiReply::new(201, order.data(now));
            let mut values = vec![integer(now)?];
            values.extend(subject.bind());
            values.extend([order.plan_id.as_str().into(), integer(order.plan_version)?]);
            let audit_id = new_id();
            let batch = vec![
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
                Audit {
                    id: &audit_id,
                    calling_service: write.key.calling_service,
                    actor_user_id: (!write.key.actor_user_id.is_empty())
                        .then_some(write.key.actor_user_id),
                    operation: write.key.operation,
                    target_id: &order.id,
                    product: write.key.product,
                    owner: write.key.owner,
                    request_id,
                    details: &json!({"planId": order.plan_id, "planVersion": order.plan_version}),
                    created_at: now,
                }
                .prepare(self.db)?,
            ];
            match write.commit(self.db, batch, &audit_id, &reply, now).await {
                Ok(_) => return Ok(reply),
                Err(CommitFailure::GuardRejected) => now = self.retry_time(now).await?,
                Err(CommitFailure::Database) => {
                    return write.recover(self.db, self.now).await;
                }
            }
        }
        if let Some(reply) = write.replay(self.db, self.now).await? {
            return Ok(reply);
        }
        catalog::find_quote(self.db, subject.product, subject.owner.kind, &body.plan_id).await?;
        Err(ApiError::VERSION_CONFLICT)
    }

    /// Persist expired orders in bounded batches. Reads enforce the deadline
    /// even if the scheduled caller hasn't swept the pending row yet.
    pub(crate) async fn expire_pending(
        self,
        log: &MaintenanceLog<'_>,
        deadline: i64,
    ) -> ApiResult<BatchResult> {
        let rows = Order::pending_expired(self.db, self.now, CLEANUP_BATCH).await?;
        let mut result = BatchResult::default();
        for order in rows {
            if storage::now() >= deadline {
                break;
            }
            match self.expire(&order, log.request_id, self.now).await {
                Ok(changed) => result.processed += usize::from(changed),
                Err(error) => {
                    result.failed += 1;
                    log.failure("orders.expire", &error);
                }
            }
        }
        Ok(result)
    }

    pub(super) async fn expire(self, order: &Order, request_id: &str, now: i64) -> ApiResult<bool> {
        let owner = order.owner();
        let audit_id = new_id();
        let values = [
            order.id.as_str().into(),
            integer(order.version)?,
            integer(now)?,
        ];
        let batch = vec![
            storage::guard(
                self.db,
                "EXISTS (SELECT 1 FROM billing_order
                   WHERE id=? AND version=? AND status='PENDING_PAYMENT' AND payment_expires_at<=?)",
                &values,
            )?,
            prepare(
                self.db,
                "UPDATE billing_order SET status='EXPIRED', version=version+1 WHERE id=?",
                &[order.id.as_str().into()],
            )?,
            Audit {
                id: &audit_id,
                calling_service: "billing",
                actor_user_id: None,
                operation: "billing.order.expire",
                target_id: &order.id,
                product: &order.product,
                owner: &owner,
                request_id,
                details: &json!({"paymentExpiresAt": order.payment_expires_at}),
                created_at: now,
            }.prepare(self.db)?,
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

pub(super) struct UserWrite<'a> {
    pub(super) key: ReceiptKey<'a>,
    hash: String,
}

impl<'a> UserWrite<'a> {
    pub(super) fn new(
        subject: Subject<'a>,
        user: &'a VerifiedUser,
        operation: &'a str,
        key: &'a str,
        target: &str,
        parameters: &Value,
    ) -> Self {
        let key = ReceiptKey {
            calling_service: "auth",
            actor_user_id: &user.user_id,
            operation,
            product: subject.product,
            owner: subject.owner,
            key,
        };
        let hash = key.request_hash(target, parameters);
        Self { key, hash }
    }

    pub(super) async fn replay(&self, db: &D1Database, now: i64) -> ApiResult<Option<ApiReply>> {
        self.replay_receipt(self.key.lookup(db, now, &self.hash).await?)
    }

    pub(super) fn replay_receipt(&self, receipt: Option<Receipt>) -> ApiResult<Option<ApiReply>> {
        Receipt::verify(receipt, &self.hash)?
            .map(|row| ApiReply::stored(row.response_status, &row.response_json))
            .transpose()
    }

    async fn recover(&self, db: &D1Database, now: i64) -> ApiResult<ApiReply> {
        self.replay(db, now)
            .await?
            .ok_or(ApiError::BILLING_UNAVAILABLE)
    }

    pub(super) async fn commit(
        &self,
        db: &D1Database,
        batch: Vec<D1PreparedStatement>,
        audit_id: &str,
        reply: &ApiReply,
        now: i64,
    ) -> Result<(), CommitFailure> {
        self.key
            .commit(
                db,
                batch,
                Success {
                    request_hash: &self.hash,
                    response_status: reply.status,
                    response: &reply.body,
                    audit_id,
                    created_at: now,
                    expires_at: now + RECEIPT_TTL_MS,
                },
            )
            .await
    }
}
