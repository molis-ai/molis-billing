#[cfg(feature = "mock-payment")]
mod mock;

use std::{future::Future, pin::Pin};

use serde::Serialize;

use super::*;
use model::{ServicePeriod, Status};

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
enum Kind {
    Payment,
    Refund,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Payment => "PAYMENT",
            Self::Refund => "REFUND",
        }
    }

    fn operation(self) -> &'static str {
        match self {
            Self::Payment => "order.pay",
            Self::Refund => "order.refund",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Event {
    event_id: String,
    kind: Kind,
    payment_id: String,
    order_id: String,
    amount_minor: i64,
    currency: String,
}

/// An adapter supplies trusted confirmations and authenticates external notices.
/// Business eligibility and all D1 changes belong to Orders.
trait PaymentProvider {
    fn name(&self) -> &'static str;
    fn confirm(&self, order: &Order, kind: Kind) -> Event;
    fn verify<'a>(&self, request: &'a mut Request, env: &'a Env) -> Verification<'a>;
    fn accepts(&self, event: &Event) -> bool;
}

type Verification<'a> = Pin<Box<dyn Future<Output = ApiResult<Event>> + 'a>>;

fn active_adapter(_env: &Env) -> Option<&'static dyn PaymentProvider> {
    #[cfg(feature = "mock-payment")]
    if mock::enabled(_env) {
        return Some(&mock::Adapter);
    }
    None
}

pub async fn webhook(
    orders: Orders<'_>,
    provider: &str,
    mut request: Request,
    env: &Env,
) -> worker::Result<Response> {
    let Some(adapter) = active_adapter(env).filter(|adapter| adapter.name() == provider) else {
        return ApiError::NOT_FOUND.response();
    };
    let request_id = request_id(&request);
    let event = match adapter.verify(&mut request, env).await {
        Ok(event) => event,
        Err(error) => return error.response(),
    };
    let result = orders.apply_event(adapter, &event, None, &request_id).await;
    respond(result)
}

impl Orders<'_> {
    pub(super) async fn confirm(
        self,
        user: &VerifiedUser,
        request: &Request,
        env: &Env,
        id: &str,
        request_id: &str,
        refund: bool,
    ) -> ApiResult<Reply> {
        let adapter = active_adapter(env).ok_or(if refund {
            ApiError::BILLING_UNAVAILABLE
        } else {
            ApiError::NOT_FOUND
        })?;
        let key = idempotency::header(request)?;
        let kind = if refund { Kind::Refund } else { Kind::Payment };
        let subject = Subject {
            product: &user.product,
            owner: &user.owner,
        };
        let write = UserWrite::new(subject, user, kind.operation(), &key, id, &json!({})).await?;
        if let Some(reply) = write.replay(self.db, self.now()).await? {
            return Ok(reply);
        }
        let order = Order::find(self.db, subject, id).await?;
        let event = adapter.confirm(&order, kind);
        self.apply_event(adapter, &event, Some(&write), request_id)
            .await
    }

    async fn apply_event(
        self,
        adapter: &dyn PaymentProvider,
        event: &Event,
        write: Option<&UserWrite<'_>>,
        request_id: &str,
    ) -> ApiResult<Reply> {
        if !valid_id(&event.event_id) || !valid_id(&event.order_id) || !valid_id(&event.payment_id)
        {
            return Err(ApiError::INVALID_REQUEST);
        }
        let record = PaymentEvent::new(adapter.name(), event).await?;
        let mut clock_floor = 0;
        for _ in 0..MAX_ATTEMPTS {
            if let Some(write) = write
                && let Some(reply) = write.replay(self.db, self.now()).await?
            {
                return Ok(reply);
            }
            let existing = record.lookup(self.db).await?;
            if write.is_none()
                && let Some(row) = &existing
            {
                return Reply::stored(200, &row.response_json);
            }
            let record_exists = existing.is_some();
            let mut order = if let Some(write) = write {
                let subject = Subject {
                    product: write.key.product,
                    owner: write.key.owner,
                };
                Order::find(self.db, subject, &event.order_id).await?
            } else {
                Order::for_event(self.db, &event.order_id).await?
            };
            let now = self.now().max(clock_floor);
            if event.amount_minor != order.amount_minor
                || event.currency != order.currency
                || !adapter.accepts(event)
                || order
                    .payment_provider
                    .as_deref()
                    .is_some_and(|name| name != adapter.name())
                || order
                    .provider_payment_id
                    .as_deref()
                    .is_some_and(|id| id != event.payment_id)
            {
                return Err(ApiError::INVALID_REQUEST);
            }

            let owner = order.owner();
            let product = order.product.clone();
            let subject = Subject {
                product: &product,
                owner: &owner,
            };
            let system = receipt_for_provider(adapter.name(), event.kind.operation(), subject);
            let scope = write.map_or(&system, |write| &write.key);
            let audit_id = new_id();
            let mut batch = Vec::new();
            let replay = match existing {
                Some(row) => Some(Reply::stored(200, &row.response_json)?),
                None => record.original_result(self.db, order.status).await?,
            };
            let replayed = replay.is_some();
            let reply = if let Some(replay) = replay {
                if let Some(write) = write
                    && event.kind == Kind::Refund
                {
                    // The first lookup may have preceded a concurrent commit.
                    return write
                        .replay(self.db, self.now())
                        .await?
                        .ok_or(ApiError::ORDER_NOT_REFUNDABLE);
                }
                replay
            } else {
                match event.kind {
                    Kind::Payment => {
                        if order.status_at(now) == Status::Expired {
                            if order.status == Status::PendingPayment
                                && !self.expire(&order, request_id, now).await?
                            {
                                continue;
                            }
                            return Err(ApiError::ORDER_EXPIRED);
                        }
                        let timeline = Timeline::load(self.db, subject, now).await?;
                        if let Some(future) = &timeline.future {
                            if future.order_id == order.id {
                                // Our own fulfillment appeared after the order read.
                                continue;
                            }
                            return Err(ApiError::RENEWAL_ALREADY_PURCHASED);
                        }
                        timeline.require_plan(&order.plan_id)?;
                        let first_credit = self.stage_payment(
                            &mut batch,
                            &mut order,
                            &record,
                            timeline.current.as_ref(),
                            now,
                        )?;
                        if let Some((anchor, period)) = first_credit.as_ref() {
                            (self.append_first_credit)(
                                self.db,
                                &FirstCredit {
                                    order_id: &event.order_id,
                                    product: &product,
                                    owner: &owner,
                                    anchor,
                                    period: *period,
                                    monthly_credits: order.monthly_credits,
                                    now,
                                    audit_id: &audit_id,
                                    request_id,
                                },
                                &mut batch,
                            )?;
                        }
                    }
                    Kind::Refund => self.stage_refund(&mut batch, &mut order, now)?,
                }
                Reply::ok(order.data(now))
            };

            // The audit must precede any credit ledger row that references it.
            batch.insert(
                0,
                audit(
                    self.db,
                    &audit_id,
                    scope,
                    &order.id,
                    request_id,
                    &json!({"provider": record.provider, "paymentId": event.payment_id,
                        "eventId": event.event_id, "amountMinor": event.amount_minor,
                        "currency": event.currency, "replay": replayed}),
                    now,
                )?,
            );
            if !record_exists {
                batch.push(record.insert(self.db, &reply, &audit_id, now)?);
            }
            if let Some(write) = write {
                write.append(self.db, &mut batch, &audit_id, &reply, now)?;
            }
            match storage::commit(self.db, batch).await {
                Ok(_) => return Ok(reply),
                Err(CommitFailure::GuardRejected) => {
                    clock_floor = self.now_after_guard_miss(now).await?;
                }
                Err(CommitFailure::Database) => {
                    if let Some(reply) = record.recover(self.db, write, self.now()).await? {
                        return Ok(reply);
                    }
                    // A callback may have won the event insert while a confirmation
                    // still needs a user receipt. Re-enter through the stored event.
                    if write.is_some() && record.lookup(self.db).await?.is_some() {
                        continue;
                    }
                    return Err(ApiError::BILLING_UNAVAILABLE);
                }
            }
        }
        if let Some(reply) = record.recover(self.db, write, self.now()).await? {
            return Ok(reply);
        }
        Err(ApiError::VERSION_CONFLICT)
    }

    fn stage_payment(
        self,
        batch: &mut Vec<D1PreparedStatement>,
        order: &mut Order,
        record: &PaymentEvent<'_>,
        current: Option<&ServicePeriod>,
        now: i64,
    ) -> ApiResult<Option<(Anchor, Period)>> {
        if order.status != Status::PendingPayment {
            return Err(ApiError::VERSION_CONFLICT);
        }
        let current_anchor = current.map(ServicePeriod::anchor);
        let renewal = current
            .zip(current_anchor.as_ref())
            .map(|(current, anchor)| calendar::Renewal {
                anchor,
                current: current.calendar_period(),
            });
        let (anchor, period) =
            calendar::purchase_period(now, &order.billing_time_zone, order.interval, renewal)?;
        let owner = order.owner();
        let subject = Subject {
            product: &order.product,
            owner: &owner,
        };
        let mut values = vec![
            number(now)?,
            order.id.as_str().into(),
            number(order.version)?,
        ];
        values.extend(subject.bind());
        values.push(current.map_or(JsValue::NULL, |p| p.order_id.as_str().into()));
        batch.push(self.time_guard(
            "EXISTS (SELECT 1 FROM billing_order WHERE id=?2 AND version=?3
               AND product=?4 AND owner_kind=?5 AND owner_id=?6 AND status='PENDING_PAYMENT'
               AND created_at<=?1 AND payment_expires_at>now)
             AND NOT EXISTS (SELECT 1 FROM billing_order_service_period WHERE
               product=?4 AND owner_kind=?5 AND owner_id=?6 AND revoked_at IS NULL AND ends_at>now
               AND order_id IS NOT ?7)
             AND (?7 IS NULL OR EXISTS (SELECT 1 FROM billing_order_service_period WHERE
               order_id=?7 AND revoked_at IS NULL AND starts_at<=now AND ends_at>now))",
            &values,
        )?);
        batch.push(prepare(
            self.db,
            "UPDATE billing_order SET status='PAID', paid_at=?, payment_provider=?,
                 provider_payment_id=?, version=version+1 WHERE id=?",
            &[
                number(now)?,
                record.provider.into(),
                record.event.payment_id.as_str().into(),
                order.id.as_str().into(),
            ],
        )?);
        let subscription_id = if let Some(period) = current {
            period.subscription_id.clone()
        } else {
            let id = new_id();
            batch.push(prepare(
                self.db,
                "INSERT INTO billing_subscription
                (id, product, owner_kind, owner_id, plan_id, billing_time_zone, anchor_at)
                VALUES (?, ?, ?, ?, ?, ?, ?)",
                &[
                    id.as_str().into(),
                    subject.product.into(),
                    owner.kind.as_str().into(),
                    owner.id.as_str().into(),
                    order.plan_id.as_str().into(),
                    anchor.time_zone.as_str().into(),
                    number(anchor.at)?,
                ],
            )?);
            id
        };
        batch.push(prepare(
            self.db,
            "INSERT INTO billing_order_service_period
            (order_id, subscription_id, product, owner_kind, owner_id, plan_id,
             month_offset, month_count, starts_at, ends_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                order.id.as_str().into(),
                subscription_id.as_str().into(),
                subject.product.into(),
                owner.kind.as_str().into(),
                owner.id.as_str().into(),
                order.plan_id.as_str().into(),
                period.month_offset.into(),
                period.month_count.into(),
                number(period.starts_at)?,
                number(period.ends_at)?,
            ],
        )?);
        order.status = Status::Paid;
        order.paid_at = Some(now);
        order.starts_at = Some(period.starts_at);
        order.ends_at = Some(period.ends_at);
        Ok((period.starts_at == now).then_some((anchor, period)))
    }

    fn stage_refund(
        self,
        batch: &mut Vec<D1PreparedStatement>,
        order: &mut Order,
        now: i64,
    ) -> ApiResult<()> {
        if !order.refundable(now) {
            return Err(ApiError::ORDER_NOT_REFUNDABLE);
        }
        batch.push(self.time_guard(
            "EXISTS (SELECT 1 FROM billing_order o JOIN billing_order_service_period p ON p.order_id=o.id
               WHERE o.id=?2 AND o.version=?3 AND o.status='PAID' AND p.revoked_at IS NULL
                 AND p.starts_at>now AND o.paid_at<=?1)",
            &[number(now)?, order.id.as_str().into(), number(order.version)?],
        )?);
        batch.push(prepare(
            self.db,
            "UPDATE billing_order
            SET status='REFUNDED', refunded_at=?, version=version+1 WHERE id=?",
            &[number(now)?, order.id.as_str().into()],
        )?);
        batch.push(prepare(
            self.db,
            "UPDATE billing_order_service_period
            SET revoked_at=?, version=version+1 WHERE order_id=?",
            &[number(now)?, order.id.as_str().into()],
        )?);
        order.status = Status::Refunded;
        order.refunded_at = Some(now);
        order.revoked_at = Some(now);
        Ok(())
    }
}

#[derive(Deserialize)]
struct StoredEvent {
    request_hash: String,
    response_json: String,
}

struct PaymentEvent<'a> {
    provider: &'a str,
    event: &'a Event,
    hash: String,
}

impl<'a> PaymentEvent<'a> {
    async fn new(provider: &'a str, event: &'a Event) -> ApiResult<Self> {
        let payload = json!([
            event.kind,
            event.payment_id,
            event.order_id,
            event.amount_minor,
            event.currency
        ]);
        let hash = db_result(idempotency::digest(&payload).await)?;
        Ok(Self {
            provider,
            event,
            hash,
        })
    }

    async fn lookup(&self, db: &D1Database) -> ApiResult<Option<StoredEvent>> {
        let found: Option<StoredEvent> = db_result(
            prepare(
                db,
                "SELECT request_hash, response_json FROM billing_payment_event
             WHERE provider=? AND external_event_id=?",
                &[self.provider.into(), self.event.event_id.as_str().into()],
            )?
            .first(None)
            .await,
        )?;
        if found
            .as_ref()
            .is_some_and(|row| row.request_hash != self.hash)
        {
            return Err(ApiError::IDEMPOTENCY_CONFLICT);
        }
        Ok(found)
    }

    async fn original_result(&self, db: &D1Database, status: Status) -> ApiResult<Option<Reply>> {
        let already_applied = match self.event.kind {
            Kind::Payment => matches!(status, Status::Paid | Status::Refunded),
            Kind::Refund => status == Status::Refunded,
        };
        if !already_applied {
            return Ok(None);
        }
        let body: Option<String> = db_result(prepare(db,
            "SELECT response_json FROM billing_payment_event WHERE provider=? AND order_id=?
               AND event_kind=? AND provider_payment_id=? ORDER BY received_at, external_event_id LIMIT 1",
            &[
                self.provider.into(), self.event.order_id.as_str().into(), self.event.kind.as_str().into(),
                self.event.payment_id.as_str().into(),
            ],
        )?.first(Some("response_json")).await)?;
        Reply::stored(200, &body.ok_or(ApiError::BILLING_UNAVAILABLE)?).map(Some)
    }

    async fn recover(
        &self,
        db: &D1Database,
        write: Option<&UserWrite<'_>>,
        now: i64,
    ) -> ApiResult<Option<Reply>> {
        if let Some(write) = write {
            write.replay(db, now).await
        } else {
            self.lookup(db)
                .await?
                .map(|row| Reply::stored(200, &row.response_json))
                .transpose()
        }
    }

    fn insert(
        &self,
        db: &D1Database,
        reply: &Reply,
        audit_id: &str,
        now: i64,
    ) -> ApiResult<D1PreparedStatement> {
        let event = self.event;
        prepare(
            db,
            "INSERT INTO billing_payment_event (provider, external_event_id, event_kind,
            provider_payment_id, order_id, amount_minor, currency, request_hash, response_json,
            audit_id, occurred_at, received_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                self.provider.into(),
                event.event_id.as_str().into(),
                event.kind.as_str().into(),
                event.payment_id.as_str().into(),
                event.order_id.as_str().into(),
                number(event.amount_minor)?,
                event.currency.as_str().into(),
                self.hash.as_str().into(),
                JsValue::from_str(&reply.body.to_string()),
                audit_id.into(),
                number(now)?,
                number(now)?,
            ],
        )
    }
}

fn receipt_for_provider<'a>(
    provider: &'a str,
    operation: &'a str,
    subject: Subject<'a>,
) -> ReceiptKey<'a> {
    ReceiptKey {
        calling_service: provider,
        actor_user_id: "",
        operation,
        product: subject.product,
        owner: subject.owner,
        key: "",
    }
}
