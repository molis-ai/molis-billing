use super::*;
#[derive(Deserialize)]
pub(super) struct StoredEvent {
    pub(super) request_hash: String,
    pub(super) response_json: String,
}

pub(super) struct PaymentEvent<'a> {
    pub(super) provider: &'a str,
    pub(super) event: &'a Event,
    hash: String,
}

impl<'a> PaymentEvent<'a> {
    pub(super) fn new(provider: &'a str, event: &'a Event) -> Self {
        let payload = json!([
            event.kind,
            event.payment_id,
            event.order_id,
            event.amount_minor,
            event.currency
        ]);
        Self {
            provider,
            event,
            hash: idempotency::digest(&payload),
        }
    }

    pub(super) async fn lookup(&self, db: &D1Database) -> ApiResult<Option<StoredEvent>> {
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
        self.verify(found.as_ref())?;
        Ok(found)
    }

    pub(super) fn verify(&self, found: Option<&StoredEvent>) -> ApiResult<()> {
        if found.is_some_and(|row| row.request_hash != self.hash) {
            return Err(ApiError::IDEMPOTENCY_CONFLICT);
        }
        Ok(())
    }

    pub(super) fn original_result(
        &self,
        result: &worker::D1Result,
        status: Status,
    ) -> ApiResult<Option<Reply>> {
        let already_applied = match self.event.kind {
            Kind::Payment => matches!(status, Status::Paid | Status::Refunded),
            Kind::Refund => status == Status::Refunded,
        };
        if !already_applied {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct Original {
            response_json: String,
        }
        let body: Original = storage::first_row(result)?.ok_or(ApiError::BILLING_UNAVAILABLE)?;
        Reply::stored(200, &body.response_json).map(Some)
    }

    pub(super) async fn recover(
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

    pub(super) fn insert(
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
