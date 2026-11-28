use serde::Deserialize;
use serde_json::{Value, json};
use worker::{D1Database, Request};

use crate::{
    app::Route,
    core::{
        http::{ApiError, ApiReply, ApiResult, generated_id as new_id, read_json},
        idempotency::{self, Audit, RECEIPT_TTL_MS, ReceiptKey, Success},
        storage::{self, CommitFailure, MAX_INTEGER, prepare},
    },
};

use super::super::{
    model::{CreditPeriod, LedgerEntry, optional},
    periods::{self, sync_credit_periods},
    snapshot,
};
use super::{
    holds::{HoldRow, find_hold, read_hold},
    types::{
        ConsumeRequest, HoldRequest, INSUFFICIENT, INVALID, Operation, Segment, SettleRequest,
        Usage,
    },
};

#[derive(Deserialize)]
struct AuditDetails {
    details_json: String,
}

pub(super) struct Write<'a> {
    pub(super) db: &'a D1Database,
    pub(super) usage: &'a Usage,
    pub(super) request_id: &'a str,
    pub(super) idempotency_key: &'a str,
    pub(super) now: i64,
}

pub(in crate::credit) async fn handle(
    route: &Route<'_>,
    mut request: Request,
    db: &D1Database,
    now: i64,
    request_id: &str,
) -> ApiResult<ApiReply> {
    let idempotency_key = idempotency::header(&request)?;

    let body: Value = read_json(&mut request).await?.ok_or(INVALID)?;
    let make = |usage| Write {
        db,
        usage,
        idempotency_key: &idempotency_key,
        request_id,
        now,
    };
    match route {
        Route::CreateHold => {
            let input: HoldRequest = parse(&body)?;
            input.usage.validate(now)?;
            check_amount(input.amount)?;
            if !(input.usage.occurred_at + 1..=MAX_INTEGER).contains(&input.expires_at) {
                return Err(INVALID);
            }
            make(&input.usage)
                .run(Operation::Hold(&input), &body, None)
                .await
        }
        Route::Consume => {
            let input: ConsumeRequest = parse(&body)?;
            input.usage.validate(now)?;
            check_amount(input.amount)?;
            make(&input.usage)
                .run(Operation::Consume(&input), &body, None)
                .await
        }
        Route::SettleHold(id) => {
            let input: SettleRequest = parse(&body)?;
            input.usage.validate(now)?;
            make(&input.usage).settle(id, &input.segments, &body).await
        }
        Route::ReleaseHold(id) => {
            let usage: Usage = parse(&body)?;
            usage.validate(now)?;
            make(&usage).run(Operation::Release(id), &body, None).await
        }
        _ => Err(ApiError::NOT_FOUND),
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &Value) -> ApiResult<T> {
    serde_json::from_value(body.clone()).map_err(|_| INVALID)
}

fn check_amount(amount: i64) -> ApiResult<()> {
    if (1..=MAX_INTEGER).contains(&amount) {
        Ok(())
    } else {
        Err(INVALID)
    }
}

impl Write<'_> {
    async fn settle(&self, id: &str, segments: &[Segment], body: &Value) -> ApiResult<ApiReply> {
        if segments.len() > 16
            || segments.is_empty()
            || segments
                .windows(2)
                .any(|rows| rows[0].occurred_at > rows[1].occurred_at)
        {
            return Err(INVALID);
        }
        for segment in segments {
            check_amount(segment.amount)?;
            if !(0..=self.now).contains(&segment.occurred_at) {
                return Err(INVALID);
            }
        }
        let mut settled = Vec::with_capacity(segments.len());
        let mut hold = Value::Null;
        for (index, segment) in segments.iter().enumerate() {
            match self
                .run(
                    Operation::Settle(id, *segment),
                    body,
                    Some((index, index + 1 == segments.len())),
                )
                .await
            {
                Ok(reply) => {
                    settled.push(reply.body["data"]["settledSegments"][0].clone());
                    hold = reply.body["data"]["hold"].clone();
                }
                Err(error) if error == INSUFFICIENT => {
                    return Err(error
                        .with_data(json!({"settledSegments": settled, "failedSegment": segment})));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(ApiReply::ok(
            json!({ "hold": hold, "settledSegments": settled }),
        ))
    }
}

impl Write<'_> {
    pub(super) async fn run(
        &self,
        operation: Operation<'_>,
        body: &Value,
        segment: Option<(usize, bool)>,
    ) -> ApiResult<ApiReply> {
        let target = match operation {
            Operation::Settle(id, _) | Operation::Release(id) => id,
            _ => &self.usage.reference_id,
        };
        let name = segment.map_or_else(
            || operation.kind().to_owned(),
            |(index, _)| format!("{}_{index}", operation.kind()),
        );
        let key = ReceiptKey {
            calling_service: &self.usage.calling_service,
            actor_user_id: &self.usage.actor_user_id,
            operation: &name,
            product: &self.usage.product,
            owner: &self.usage.owner,
            key: self.idempotency_key,
        };
        let hash = key.request_hash(target, body);
        // Successful segments remain anchored to the immutable audit entry after receipt cleanup.
        let durable_audit = if segment.is_some() {
            let digest = idempotency::digest(&json!([
                key.calling_service,
                key.actor_user_id,
                key.operation,
                key.product,
                key.owner,
                key.key,
            ]));
            Some(format!("credit-segment-{digest}"))
        } else {
            None
        };
        let scope = self.usage.scope();
        for _ in 0..4 {
            let hold_id = match operation {
                Operation::Settle(id, _) | Operation::Release(id) => Some(id),
                _ => None,
            };
            let at = match operation {
                Operation::Settle(_, segment) => segment.occurred_at,
                _ => self.usage.occurred_at,
            };
            let [timeout, archive, grant] = snapshot::probes(self.db, &scope, self.now)?;
            let lookup_id = hold_id.unwrap_or("");
            let audit_read = prepare(
                self.db,
                "SELECT details_json FROM billing_audit WHERE id=?",
                &[optional(durable_audit.as_deref())],
            )?;
            let held_period_read = prepare(
                self.db, "SELECT p.* FROM billing_credit_period p JOIN billing_credit_hold h
                      ON h.credit_period_id=p.id WHERE h.id=? AND h.product=? AND h.owner_kind=? AND h.owner_id=?",
                &[lookup_id.into(),scope.product.into(),scope.owner.kind.as_str().into(),scope.owner.id.as_str().into()],
            )?;
            let [
                receipt,
                audit,
                timeout,
                archive,
                grant,
                hold,
                held_period,
                period,
            ] = storage::batch_read(
                self.db,
                [
                    key.read(self.db, self.now)?,
                    audit_read,
                    timeout,
                    archive,
                    grant,
                    read_hold(self.db, &scope, lookup_id)?,
                    held_period_read,
                    periods::read_period(self.db, &scope, at)?,
                ],
            )
            .await?;
            let row: Option<HoldRow> = storage::first_row(&hold)?;
            if hold_id.is_some() {
                let row = row.as_ref().ok_or(ApiError::NOT_FOUND)?;
                row.verify(self.usage)?;
                if matches!(operation, Operation::Settle(..)) && at < row.occurred_at {
                    return Err(INVALID);
                }
            }
            let receipt = idempotency::Receipt::verify(storage::first_row(&receipt)?, &hash)?;
            let replay = if let Some(receipt) = receipt {
                Some(ApiReply::stored(
                    receipt.response_status,
                    &receipt.response_json,
                )?)
            } else if let Some(audit) = storage::first_row::<AuditDetails>(&audit)? {
                let details: Value = serde_json::from_str(&audit.details_json)
                    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
                if details["requestHash"] != hash {
                    return Err(ApiError::IDEMPOTENCY_CONFLICT);
                }
                Some(ApiReply {
                    status: 200,
                    body: details["result"].clone(),
                })
            } else {
                None
            };
            if !matches!(operation, Operation::Settle(..))
                && let Some(reply) = replay
            {
                return Ok(reply);
            }
            let held_period: Option<CreditPeriod> = storage::first_row(&held_period)?;
            let same_period = matches!(operation,Operation::Settle(_,seg) if held_period.as_ref().is_some_and(|period|seg.occurred_at<period.expires_at));
            let pending = snapshot::due([&timeout, &archive, &grant])?;
            if pending && (!same_period || replay.is_some()) {
                sync_credit_periods(self.db, &scope, self.now, self.request_id).await?;
                continue;
            }
            if let Some(mut reply) = replay {
                if let Some(row) = row {
                    reply.body["data"]["hold"] = json!(row.snapshot());
                }
                return Ok(reply);
            }
            let period = storage::first_row(&period)?;

            let planned = match operation {
                Operation::Hold(input) => self.open(input, period),
                Operation::Consume(input) => self.consume(at, input.amount, period).await,
                Operation::Release(_) => self.release(row.ok_or(ApiError::NOT_FOUND)?, held_period),
                Operation::Settle(_, segment) => {
                    self.settle_one(
                        row.ok_or(ApiError::NOT_FOUND)?,
                        segment,
                        held_period,
                        period,
                    )
                    .await
                }
            };
            let mutation = match planned {
                Ok(mutation) => mutation,
                Err(error) => {
                    if let Some(response) =
                        self.replay(&key, &hash, durable_audit.as_deref()).await?
                    {
                        return Ok(response);
                    }
                    if error == ApiError::VERSION_CONFLICT {
                        continue;
                    }
                    return Err(error);
                }
            };
            let audit_id = durable_audit.clone().unwrap_or_else(new_id);
            let ledger_id = new_id();
            let status = if matches!(operation, Operation::Hold(_)) {
                201
            } else {
                200
            };
            let mut response = if status == 201 {
                json!({ "data": { "hold": mutation.hold } })
            } else {
                let settled = mutation.amount.map(|amount| {
                    json!({
                        "occurredAt": mutation.occurred_at, "amount": amount, "ledgerId": ledger_id
                    })
                });
                json!({ "data": { "hold": mutation.hold, "settledSegments": settled.into_iter().collect::<Vec<_>>() } })
            };
            let detail = json!({
                "referenceId": self.usage.reference_id, "requestHash": hash, "result": response,
            });
            let audit = Audit {
                id: &audit_id,
                calling_service: &self.usage.calling_service,
                actor_user_id: Some(&self.usage.actor_user_id),
                operation: operation.kind(),
                target_id: mutation.hold.as_ref().map_or(target, |hold| &hold.id),
                product: &self.usage.product,
                owner: &self.usage.owner,
                request_id: self.request_id,
                details: &detail,
                created_at: self.now,
            };
            let mut batch = mutation.statements;
            batch.push(audit.prepare(self.db)?);
            if let Some(period_id) = &mutation.period_id {
                batch.push(
                    LedgerEntry {
                        id: &ledger_id,
                        period_id,
                        hold_id: mutation
                            .hold
                            .as_ref()
                            .filter(|_| mutation.ledger_hold)
                            .map(|hold| hold.id.as_str()),
                        kind: mutation.kind,
                        delta: mutation.delta,
                        reference: Some(&self.usage.reference_id),
                        actor: Some(&self.usage.actor_user_id),
                        occurred_at: mutation.occurred_at,
                    }
                    .prepare(self.db, &audit)?,
                );
            }
            let committed = key
                .commit(
                    self.db,
                    batch,
                    Success {
                        request_hash: &hash,
                        response_status: status,
                        response: &response,
                        audit_id: &audit_id,
                        created_at: self.now,
                        expires_at: self.now + RECEIPT_TTL_MS,
                    },
                )
                .await;
            match committed {
                Ok(_) => {
                    if pending
                        && segment.is_some_and(|(_, last)| last)
                        && let Some(id) = hold_id
                    {
                        sync_credit_periods(self.db, &scope, self.now, self.request_id).await?;
                        response["data"]["hold"] =
                            json!(find_hold(self.db, &scope, id).await?.snapshot());
                    }
                    return Ok(ApiReply {
                        status,
                        body: response,
                    });
                }
                Err(error) => {
                    if let Some(response) =
                        self.replay(&key, &hash, durable_audit.as_deref()).await?
                    {
                        return Ok(response);
                    }
                    if error != CommitFailure::GuardRejected {
                        return Err(error.into_api_error());
                    }
                }
            }
        }
        Err(ApiError::VERSION_CONFLICT)
    }

    async fn replay(
        &self,
        key: &ReceiptKey<'_>,
        hash: &str,
        audit_id: Option<&str>,
    ) -> ApiResult<Option<ApiReply>> {
        if let Some(receipt) = key.lookup(self.db, self.now, hash).await? {
            return ApiReply::stored(receipt.response_status, &receipt.response_json).map(Some);
        }
        if let Some(audit_id) = audit_id {
            let details = prepare(
                self.db,
                "SELECT details_json FROM billing_audit WHERE id=?",
                &[audit_id.into()],
            )?
            .first::<String>(Some("details_json"))
            .await
            .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
            if let Some(details) = details {
                let details: Value =
                    serde_json::from_str(&details).map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
                if details["requestHash"].as_str() != Some(hash) {
                    return Err(ApiError::IDEMPOTENCY_CONFLICT);
                }
                return Ok(Some(ApiReply {
                    status: 200,
                    body: details["result"].clone(),
                }));
            }
        }
        Ok(None)
    }
}
