mod records;
use records::{PaymentEvent, StoredEvent};
#[cfg(feature = "mock-payment")]
mod mock;
mod mutations;

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
            Self::Payment => "billing.order.pay",
            Self::Refund => "billing.order.refund",
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
    fn identifiers(&self, id: &str, kind: Kind) -> (String, String);
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
    context: &RequestContext<'_>,
) -> ApiResult<Reply> {
    let adapter = active_adapter(context.env)
        .filter(|adapter| adapter.name() == provider)
        .ok_or(ApiError::NOT_FOUND)?;
    let event = adapter.verify(&mut request, context.env).await?;
    if !valid_id(&event.event_id) || !valid_id(&event.order_id) || !valid_id(&event.payment_id) {
        return Err(ApiError::INVALID_REQUEST);
    }
    orders
        .apply_event(
            PaymentInput {
                adapter,
                order_id: &event.order_id,
                kind: event.kind,
                event_id: &event.event_id,
                payment_id: &event.payment_id,
                write: None,
                webhook: Some(&event),
            },
            context.request_id,
        )
        .await
}

struct PaymentInput<'a> {
    adapter: &'a dyn PaymentProvider,
    order_id: &'a str,
    kind: Kind,
    event_id: &'a str,
    payment_id: &'a str,
    write: Option<&'a UserWrite<'a>>,
    webhook: Option<&'a Event>,
}

impl PaymentInput<'_> {
    fn read_event(&self, db: &D1Database) -> ApiResult<D1PreparedStatement> {
        prepare(
            db,
            "SELECT request_hash, response_json FROM billing_payment_event
             WHERE provider=? AND external_event_id=?",
            &[self.adapter.name().into(), self.event_id.into()],
        )
    }

    fn read_original(&self, db: &D1Database) -> ApiResult<D1PreparedStatement> {
        prepare(
            db,
            "SELECT response_json FROM billing_payment_event
             WHERE provider=? AND order_id=? AND event_kind=? AND provider_payment_id=?
             ORDER BY received_at, external_event_id LIMIT 1",
            &[
                self.adapter.name().into(),
                self.order_id.into(),
                self.kind.as_str().into(),
                self.payment_id.into(),
            ],
        )
    }
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
        let write = UserWrite::new(subject, user, kind.operation(), &key, id, &json!({}));
        let (event_id, payment_id) = adapter.identifiers(id, kind);
        self.apply_event(
            PaymentInput {
                adapter,
                order_id: id,
                kind,
                event_id: &event_id,
                payment_id: &payment_id,
                write: Some(&write),
                webhook: None,
            },
            request_id,
        )
        .await
    }

    async fn apply_event(self, input: PaymentInput<'_>, request_id: &str) -> ApiResult<Reply> {
        let PaymentInput { adapter, write, .. } = input;
        let subject = write.map(|write| Subject {
            product: write.key.product,
            owner: write.key.owner,
        });
        let mut now = self.now;
        for _ in 0..MAX_ATTEMPTS {
            let timeline_read = match (input.kind, subject) {
                (Kind::Payment, Some(subject)) => Timeline::read(self.db, &subject.bind(), now)?,
                (Kind::Payment, None) => Timeline::for_order(self.db, input.order_id, now)?,
                (Kind::Refund, _) => self.db.prepare("SELECT NULL WHERE 0"),
            };
            let [receipt, order, existing, original, timeline] = storage::batch_read(
                self.db,
                [
                    match write {
                        Some(write) => write.key.read(self.db, now)?,
                        None => self.db.prepare("SELECT NULL WHERE 0"),
                    },
                    Order::read(self.db, subject, input.order_id)?,
                    input.read_event(self.db)?,
                    input.read_original(self.db)?,
                    timeline_read,
                ],
            )
            .await?;
            if let Some(write) = write
                && let Some(reply) = write.replay_receipt(storage::first_row(&receipt)?)?
            {
                return Ok(reply);
            }
            let existing: Option<StoredEvent> = storage::first_row(&existing)?;
            if let (Some(event), Some(row)) = (input.webhook, &existing) {
                PaymentEvent::new(adapter.name(), event).verify(Some(row))?;
                return Reply::stored(200, &row.response_json);
            }
            let mut order: Order = storage::first_row(&order)?.ok_or(ApiError::NOT_FOUND)?;
            let confirmation;
            let event = match input.webhook {
                Some(event) => event,
                None => {
                    if !valid_id(input.event_id) || !valid_id(input.payment_id) {
                        return Err(ApiError::INVALID_REQUEST);
                    }
                    confirmation = Event {
                        order_id: order.id.clone(),
                        kind: input.kind,
                        event_id: input.event_id.to_owned(),
                        payment_id: input.payment_id.to_owned(),
                        amount_minor: order.amount_minor,
                        currency: order.currency.clone(),
                    };
                    &confirmation
                }
            };
            let record = PaymentEvent::new(adapter.name(), event);
            record.verify(existing.as_ref())?;
            let record_exists = existing.is_some();
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
                None => record.original_result(&original, order.status)?,
            };
            let replayed = replay.is_some();
            let reply = if let Some(replay) = replay {
                if write.is_some() && event.kind == Kind::Refund {
                    // The receipt and event came from the same snapshot.
                    return Err(ApiError::ORDER_NOT_REFUNDABLE);
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
                        let timeline = Timeline::from_rows(storage::rows(&timeline)?, now)?;
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
                            batch.extend(prepare_paid_first_month(
                                self.db,
                                &CreditScope {
                                    product: &product,
                                    owner: &owner,
                                },
                                FirstMonthGrant {
                                    order_id: &event.order_id,
                                    anchor,
                                    period,
                                    monthly_credits: order.monthly_credits,
                                    paid_at: now,
                                    request_id,
                                },
                            )?);
                            #[cfg(feature = "test-fixture")]
                            if self.test.fail_after_grant {
                                batch.push(prepare(
                                    self.db,
                                    "UPDATE billing_credit_ledger SET created_at=created_at
                                     WHERE credit_period_id IN (SELECT id FROM billing_credit_period WHERE order_id=?)",
                                    &[event.order_id.as_str().into()],
                                )?);
                            }
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
            let committed = match write {
                Some(write) => write.commit(self.db, batch, &audit_id, &reply, now).await,
                None => storage::commit(self.db, batch).await.map(|_| ()),
            };
            match committed {
                Ok(_) => return Ok(reply),
                Err(CommitFailure::GuardRejected) => {
                    now = self.retry_time(now).await?;
                }
                Err(CommitFailure::Database) => {
                    if let Some(reply) = record.recover(self.db, write, now).await? {
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
        if let Some(write) = write
            && let Some(reply) = write.replay(self.db, now).await?
        {
            return Ok(reply);
        }
        if let Some(event) = input.webhook
            && let Some(reply) = PaymentEvent::new(adapter.name(), event)
                .recover(self.db, None, now)
                .await?
        {
            return Ok(reply);
        }
        Err(ApiError::VERSION_CONFLICT)
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
