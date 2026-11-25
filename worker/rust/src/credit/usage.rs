use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{D1Database, D1PreparedStatement, Request};

use super::{
    CreditScope, Delta, LedgerEntry, Result, UNAVAILABLE, bind, commit_failure, guard, new_id,
    number,
    periods::{CreditPeriod, period_at},
    sync_credit_periods,
};
use crate::{
    app::Route,
    http::{ApiError, request_id, valid_id},
    idempotency::{self, Audit, ReceiptKey, Success},
    identity::{Owner, OwnerKind},
    storage::{self, CommitFailure},
};

const INSUFFICIENT: ApiError = ApiError::INSUFFICIENT_CREDITS;
const INVALID: ApiError = ApiError::INVALID_REQUEST;
const MAX_INTEGER: i64 = 9_007_199_254_740_991;
const RECEIPT_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    calling_service: String,
    actor_user_id: String,
    product: String,
    owner: Owner,
    reference_id: String,
    occurred_at: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct HoldRequest {
    #[serde(flatten)]
    usage: Usage,
    amount: i64,
    expires_at: i64,
}

#[derive(Deserialize, Serialize)]
struct ConsumeRequest {
    #[serde(flatten)]
    usage: Usage,
    amount: i64,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Segment {
    occurred_at: i64,
    amount: i64,
}

#[derive(Deserialize, Serialize)]
struct SettleRequest {
    #[serde(flatten)]
    usage: Usage,
    segments: Vec<Segment>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
pub(super) struct Hold {
    pub id: String,
    pub credit_period_id: String,
    pub reference_id: String,
    pub status: &'static str,
    pub amount: i64,
    pub reserved: i64,
    pub settled: i64,
    pub released: i64,
    pub expired: i64,
    pub expires_at: i64,
}

#[derive(Deserialize)]
pub(super) struct HoldRow {
    id: String,
    credit_period_id: String,
    reference_id: String,
    pub actor_user_id: String,
    calling_service: String,
    status: HoldStatus,
    amount: i64,
    reserved: i64,
    settled: i64,
    released: i64,
    expired: i64,
    pub expires_at: i64,
    occurred_at: i64,
    version: i64,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
enum HoldStatus {
    Open,
    Closed,
    Expired,
}

impl HoldStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Closed => "CLOSED",
            Self::Expired => "EXPIRED",
        }
    }
}

impl HoldRow {
    pub(super) fn snapshot(&self) -> Hold {
        Hold {
            id: self.id.clone(),
            credit_period_id: self.credit_period_id.clone(),
            reference_id: self.reference_id.clone(),
            status: self.status.as_str(),
            amount: self.amount,
            reserved: self.reserved,
            settled: self.settled,
            released: self.released,
            expired: self.expired,
            expires_at: self.expires_at,
        }
    }

    fn verify(&self, usage: &Usage) -> Result<()> {
        if self.actor_user_id != usage.actor_user_id
            || self.calling_service != usage.calling_service
            || self.reference_id != usage.reference_id
        {
            return Err(ApiError::NOT_FOUND);
        }
        if usage.occurred_at < self.occurred_at {
            return Err(INVALID);
        }
        Ok(())
    }

    pub(super) fn update(
        &self,
        db: &D1Database,
        updated: &Hold,
        now: i64,
    ) -> Result<[D1PreparedStatement; 2]> {
        Ok([
            guard(db, "EXISTS (SELECT 1 FROM billing_credit_hold WHERE id=? AND version=?)",
                  &[self.id.as_str().into(), number(self.version)])?,
            bind(db, "UPDATE billing_credit_hold
                      SET reserved=?, settled=?, released=?, expired=?, status=?, updated_at=?, version=version+1
                      WHERE id=? AND version=?",
                 &[number(updated.reserved), number(updated.settled), number(updated.released),
                   number(updated.expired), updated.status.into(), number(now), self.id.as_str().into(),
                   number(self.version)])?,
        ])
    }
}

pub(super) async fn find_hold(
    db: &D1Database,
    scope: &CreditScope<'_>,
    id: &str,
) -> Result<HoldRow> {
    let mut values = scope.values().to_vec();
    values.push(id.into());
    bind(db, "SELECT id, credit_period_id, reference_id, actor_user_id, calling_service,
                    status, amount, reserved, settled, released, expired, expires_at, occurred_at, version
              FROM billing_credit_hold WHERE product=? AND owner_kind=? AND owner_id=? AND id=?",
         &values)?
        .first(None).await.map_err(|_| UNAVAILABLE)?.ok_or(ApiError::NOT_FOUND)
}

impl Usage {
    fn validate(&self, now: i64) -> Result<()> {
        if self.product != "WORK"
            || !valid_id(&self.calling_service)
            || self.calling_service == super::SYSTEM
            || !valid_id(&self.actor_user_id)
            || !valid_id(&self.owner.id)
            || !valid_id(&self.reference_id)
            || !(0..=now).contains(&self.occurred_at)
        {
            return Err(INVALID);
        }
        if self.owner.kind == OwnerKind::User && self.owner.id != self.actor_user_id {
            return Err(ApiError::FORBIDDEN);
        }
        Ok(())
    }

    fn scope(&self) -> CreditScope<'_> {
        CreditScope {
            owner: &self.owner,
            product: &self.product,
        }
    }
}

enum Operation<'a> {
    Hold(&'a HoldRequest),
    Consume(&'a ConsumeRequest),
    Settle(&'a str, Segment),
    Release(&'a str),
}

impl Operation<'_> {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Hold(_) => "CREDIT_HOLD",
            Self::Consume(_) => "CREDIT_CONSUME",
            Self::Settle(..) => "CREDIT_SETTLE",
            Self::Release(_) => "CREDIT_RELEASE",
        }
    }
}

struct Write<'a> {
    db: &'a D1Database,
    usage: &'a Usage,
    request_id: &'a str,
    idempotency_key: &'a str,
    now: i64,
}

struct Mutation {
    statements: Vec<D1PreparedStatement>,
    period_id: Option<String>,
    hold: Option<Hold>,
    ledger_hold: bool,
    kind: &'static str,
    delta: Delta,
    occurred_at: i64,
    amount: Option<i64>,
}

impl Mutation {
    fn debit(period: &CreditPeriod, at: i64, amount: i64, delta: Delta) -> Self {
        Self {
            statements: Vec::new(),
            period_id: Some(period.id.clone()),
            hold: None,
            ledger_hold: false,
            kind: "CONSUME",
            delta,
            occurred_at: at,
            amount: Some(amount),
        }
    }
}

pub(super) async fn handle(
    route: &Route<'_>,
    mut request: Request,
    db: &D1Database,
    now: i64,
) -> Result<(u16, Value)> {
    let idempotency_key = idempotency::header(&request)?;
    let request_id = request_id(&request);
    let body: Value = request.json().await.map_err(|_| INVALID)?;
    let make = |usage| Write {
        db,
        usage,
        idempotency_key: &idempotency_key,
        request_id: &request_id,
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

fn parse<T: serde::de::DeserializeOwned>(body: &Value) -> Result<T> {
    serde_json::from_value(body.clone()).map_err(|_| INVALID)
}

fn check_amount(amount: i64) -> Result<()> {
    if (1..=MAX_INTEGER).contains(&amount) {
        Ok(())
    } else {
        Err(INVALID)
    }
}

impl Write<'_> {
    async fn settle(&self, id: &str, segments: &[Segment], body: &Value) -> Result<(u16, Value)> {
        if segments.is_empty()
            || segments
                .windows(2)
                .any(|rows| rows[0].occurred_at > rows[1].occurred_at)
        {
            return Err(INVALID);
        }
        let row = find_hold(self.db, &self.usage.scope(), id).await?;
        row.verify(self.usage)?;
        for segment in segments {
            check_amount(segment.amount)?;
            if !(row.occurred_at..=self.now).contains(&segment.occurred_at) {
                return Err(INVALID);
            }
        }
        let mut settled = Vec::with_capacity(segments.len());
        let mut hold = Value::Null;
        for (index, segment) in segments.iter().enumerate() {
            match self
                .run(Operation::Settle(id, *segment), body, Some(index))
                .await
            {
                Ok((_, response)) => {
                    settled.push(response["data"]["settledSegments"][0].clone());
                    hold = response["data"]["hold"].clone();
                }
                Err(INSUFFICIENT) => {
                    return Ok((
                        402,
                        json!({
                            "error": {"code": INSUFFICIENT.code, "message": INSUFFICIENT.message},
                            "data": { "settledSegments": settled, "failedSegment": segment }
                        }),
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        sync_credit_periods(self.db, &self.usage.scope(), self.now, self.request_id).await?;
        Ok((
            200,
            json!({ "data": { "hold": hold, "settledSegments": settled } }),
        ))
    }

    async fn run(
        &self,
        operation: Operation<'_>,
        body: &Value,
        segment_index: Option<usize>,
    ) -> Result<(u16, Value)> {
        let target = match operation {
            Operation::Settle(id, _) | Operation::Release(id) => id,
            _ => &self.usage.reference_id,
        };
        let name = segment_index.map_or_else(
            || operation.kind().to_owned(),
            |index| format!("{}_{index}", operation.kind()),
        );
        let key = ReceiptKey {
            calling_service: &self.usage.calling_service,
            actor_user_id: &self.usage.actor_user_id,
            operation: &name,
            product: &self.usage.product,
            owner: &self.usage.owner,
            key: self.idempotency_key,
        };
        let hash = key
            .request_hash(target, body)
            .await
            .map_err(|_| UNAVAILABLE)?;
        // Successful segments remain anchored to the immutable audit entry after receipt cleanup.
        let durable_audit = if segment_index.is_some() {
            let digest = idempotency::digest(&json!([
                key.calling_service,
                key.actor_user_id,
                key.operation,
                key.product,
                key.owner,
                key.key,
            ]))
            .await
            .map_err(|_| UNAVAILABLE)?;
            Some(format!("credit-segment-{digest}"))
        } else {
            None
        };
        if let Some(response) = self.replay(&key, &hash, durable_audit.as_deref()).await? {
            return Ok(response);
        }

        for _ in 0..4 {
            let mutation = match self.plan(&operation).await {
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
            let response = if status == 201 {
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
            batch.push(audit.prepare(self.db).map_err(|_| UNAVAILABLE)?);
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
            batch.extend(
                key.success(
                    self.db,
                    Success {
                        request_hash: &hash,
                        response_status: status,
                        response: &response,
                        audit_id: &audit_id,
                        created_at: self.now,
                        expires_at: self.now + RECEIPT_TTL_MS,
                    },
                )
                .map_err(|_| UNAVAILABLE)?,
            );
            match storage::commit(self.db, batch).await {
                Ok(_) => return Ok((status, response)),
                Err(error) => {
                    if let Some(response) =
                        self.replay(&key, &hash, durable_audit.as_deref()).await?
                    {
                        return Ok(response);
                    }
                    if error != CommitFailure::GuardRejected {
                        return Err(commit_failure(error));
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
    ) -> Result<Option<(u16, Value)>> {
        if let Some(receipt) = key.lookup(self.db, self.now, hash).await? {
            let body = serde_json::from_str(&receipt.response_json).map_err(|_| UNAVAILABLE)?;
            return Ok(Some((receipt.response_status, body)));
        }
        if let Some(audit_id) = audit_id {
            let details = bind(
                self.db,
                "SELECT details_json FROM billing_audit WHERE id=?",
                &[audit_id.into()],
            )?
            .first::<String>(Some("details_json"))
            .await
            .map_err(|_| UNAVAILABLE)?;
            if let Some(details) = details {
                let details: Value = serde_json::from_str(&details).map_err(|_| UNAVAILABLE)?;
                if details["requestHash"].as_str() != Some(hash) {
                    return Err(ApiError::IDEMPOTENCY_CONFLICT);
                }
                return Ok(Some((200, details["result"].clone())));
            }
        }
        Ok(None)
    }

    async fn plan(&self, operation: &Operation<'_>) -> Result<Mutation> {
        let scope = self.usage.scope();
        if matches!(operation, Operation::Hold(_) | Operation::Consume(_)) {
            sync_credit_periods(self.db, &scope, self.now, self.request_id).await?;
        }
        match *operation {
            Operation::Hold(input) => self.open(input).await,
            Operation::Consume(input) => self.consume(input.usage.occurred_at, input.amount).await,
            Operation::Settle(id, segment) => self.settle_one(id, segment).await,
            Operation::Release(id) => self.release(id).await,
        }
    }

    async fn open(&self, input: &HoldRequest) -> Result<Mutation> {
        if input.expires_at <= self.now {
            return Err(INVALID);
        }
        let scope = self.usage.scope();
        let period = period_at(self.db, &scope, self.usage.occurred_at)
            .await?
            .ok_or(INSUFFICIENT)?;
        if period.status != "ACTIVE"
            || period.expires_at <= self.now
            || period.available < input.amount
        {
            return Err(INSUFFICIENT);
        }
        let hold = Hold {
            id: new_id(),
            credit_period_id: period.id.clone(),
            reference_id: self.usage.reference_id.clone(),
            status: "OPEN",
            amount: input.amount,
            reserved: input.amount,
            settled: 0,
            released: 0,
            expired: 0,
            expires_at: input.expires_at,
        };
        let mut values = vec![hold.id.as_str().into(), period.id.as_str().into()];
        values.extend(scope.values());
        values.extend([
            self.usage.reference_id.as_str().into(),
            self.usage.actor_user_id.as_str().into(),
            self.usage.calling_service.as_str().into(),
            self.request_id.into(),
            number(input.amount),
            number(input.amount),
            number(self.usage.occurred_at),
            number(self.now),
            number(input.expires_at),
            number(self.now),
        ]);
        let delta = Delta {
            available: -input.amount,
            reserved: input.amount,
            ..Delta::default()
        };
        let mut statements = update_period(self.db, &period, &delta, self.now)?.to_vec();
        statements.push(bind(self.db, "INSERT INTO billing_credit_hold
            (id, credit_period_id, product, owner_kind, owner_id, reference_id, actor_user_id, calling_service,
             request_id, status, amount, reserved, settled, released, expired, occurred_at, created_at, expires_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'OPEN', ?, ?, 0, 0, 0, ?, ?, ?, ?)", &values)?);
        Ok(Mutation {
            statements,
            period_id: Some(period.id),
            hold: Some(hold),
            ledger_hold: true,
            kind: "HOLD",
            delta,
            occurred_at: self.usage.occurred_at,
            amount: None,
        })
    }

    async fn consume(&self, at: i64, amount: i64) -> Result<Mutation> {
        let period = period_at(self.db, &self.usage.scope(), at)
            .await?
            .ok_or(INSUFFICIENT)?;
        let mut delta = Delta {
            consumed: amount,
            ..Delta::default()
        };
        if period.status == "EXPIRED" {
            let available = bind(
                self.db,
                "SELECT p.expired - COALESCE((SELECT SUM(h.expired) FROM billing_credit_hold h
                                            WHERE h.credit_period_id=p.id), 0) AS available
                 FROM billing_credit_period p WHERE p.id=? AND p.version=?",
                &[period.id.as_str().into(), number(period.version)],
            )?
            .first::<i64>(Some("available"))
            .await
            .map_err(|_| UNAVAILABLE)?
            .ok_or(ApiError::VERSION_CONFLICT)?;
            if available < amount {
                return Err(INSUFFICIENT);
            }
            delta.expired = -amount;
        } else {
            if period.available < amount {
                return Err(INSUFFICIENT);
            }
            delta.available = -amount;
        }
        let mut mutation = Mutation::debit(&period, at, amount, delta);
        mutation
            .statements
            .extend(update_period(self.db, &period, &mutation.delta, self.now)?);
        Ok(mutation)
    }

    async fn settle_one(&self, id: &str, segment: Segment) -> Result<Mutation> {
        let scope = self.usage.scope();
        let row = find_hold(self.db, &scope, id).await?;
        row.verify(self.usage)?;
        let period = period_at(self.db, &scope, row.occurred_at)
            .await?
            .ok_or(INSUFFICIENT)?;
        if period.id != row.credit_period_id {
            return Err(UNAVAILABLE);
        }
        if segment.occurred_at >= period.expires_at {
            if row.expires_at < period.expires_at {
                return Err(INSUFFICIENT);
            }
            sync_credit_periods(self.db, &scope, self.now, self.request_id).await?;
            let mut mutation = self.consume(segment.occurred_at, segment.amount).await?;
            // The newer period cannot use the old hold FK. The audit and reference link the task.
            mutation.hold = Some(find_hold(self.db, &scope, id).await?.snapshot());
            return Ok(mutation);
        }
        if segment.occurred_at >= row.expires_at {
            return Err(INSUFFICIENT);
        }

        let mut hold = row.snapshot();
        let mut delta = Delta {
            consumed: segment.amount,
            ..Delta::default()
        };
        match row.status {
            HoldStatus::Open
                if row.reserved >= segment.amount
                    && (self.now < row.expires_at || row.expires_at >= period.expires_at) =>
            {
                hold.reserved -= segment.amount;
                hold.status = if hold.reserved == 0 { "CLOSED" } else { "OPEN" };
                delta.reserved = -segment.amount;
            }
            HoldStatus::Expired if period.status == "EXPIRED" && row.expired >= segment.amount => {
                // A read or scheduled invocation already closed the month. Reclassify only the
                // reservation that belonged to this hold, retaining the original EXPIRE event.
                hold.expired -= segment.amount;
                if hold.expired == 0 {
                    hold.status = "CLOSED";
                }
                delta.expired = -segment.amount;
            }
            _ => return Err(INSUFFICIENT),
        }
        hold.settled += segment.amount;
        let mut statements = row.update(self.db, &hold, self.now)?.to_vec();
        statements.extend(update_period(self.db, &period, &delta, self.now)?);
        Ok(Mutation {
            statements,
            period_id: Some(period.id),
            hold: Some(hold),
            ledger_hold: true,
            kind: "SETTLE",
            delta,
            occurred_at: segment.occurred_at,
            amount: Some(segment.amount),
        })
    }

    async fn release(&self, id: &str) -> Result<Mutation> {
        let scope = self.usage.scope();
        find_hold(self.db, &scope, id).await?.verify(self.usage)?;
        sync_credit_periods(self.db, &scope, self.now, self.request_id).await?;
        let row = find_hold(self.db, &scope, id).await?;
        let mut hold = row.snapshot();
        let mut mutation = Mutation {
            statements: Vec::new(),
            period_id: None,
            hold: None,
            ledger_hold: true,
            kind: "RELEASE",
            delta: Delta::default(),
            occurred_at: self.usage.occurred_at,
            amount: None,
        };
        if row.status == HoldStatus::Open {
            let period = period_at(self.db, &scope, row.occurred_at)
                .await?
                .ok_or(UNAVAILABLE)?;
            if period.id != row.credit_period_id {
                return Err(UNAVAILABLE);
            }
            mutation.period_id = Some(period.id.clone());
            mutation.delta.available = hold.reserved;
            mutation.delta.reserved = -hold.reserved;
            hold.released += hold.reserved;
            hold.reserved = 0;
            hold.status = "CLOSED";
            mutation
                .statements
                .extend(row.update(self.db, &hold, self.now)?);
            mutation
                .statements
                .extend(update_period(self.db, &period, &mutation.delta, self.now)?);
        }
        mutation.hold = Some(hold);
        Ok(mutation)
    }
}

pub(super) fn update_period(
    db: &D1Database,
    period: &CreditPeriod,
    delta: &Delta,
    now: i64,
) -> Result<[D1PreparedStatement; 2]> {
    Ok([
        guard(db, "EXISTS (SELECT 1 FROM billing_credit_period WHERE id=? AND version=? AND status=?)",
              &[period.id.as_str().into(), number(period.version), period.status.as_str().into()])?,
        bind(db, "UPDATE billing_credit_period
                  SET available=available+?, reserved=reserved+?, consumed=consumed+?, expired=expired+?,
                      updated_at=?, version=version+1 WHERE id=? AND version=?",
             &[number(delta.available), number(delta.reserved), number(delta.consumed), number(delta.expired),
               number(now), period.id.as_str().into(), number(period.version)])?,
    ])
}
