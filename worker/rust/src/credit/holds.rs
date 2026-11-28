use super::*;

#[derive(Clone, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct Hold {
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
pub(crate) struct HoldRow {
    pub(super) id: String,
    pub(super) credit_period_id: String,
    pub(super) reference_id: String,
    pub actor_user_id: String,
    pub(super) calling_service: String,
    pub(super) status: HoldStatus,
    pub(super) amount: i64,
    pub(super) reserved: i64,
    pub(super) settled: i64,
    pub(super) released: i64,
    pub(super) expired: i64,
    pub expires_at: i64,
    pub(super) occurred_at: i64,
    pub(super) version: i64,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub(super) enum HoldStatus {
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
    pub(crate) fn snapshot(&self) -> Hold {
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

    pub(super) fn verify(&self, usage: &Usage) -> Result<()> {
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

    pub(crate) fn update(
        &self,
        db: &D1Database,
        updated: &Hold,
        now: i64,
    ) -> Result<[D1PreparedStatement; 2]> {
        Ok([
            guard(db, "EXISTS (SELECT 1 FROM billing_credit_hold WHERE id=? AND version=?)",
                  &[self.id.as_str().into(), number(self.version)?])?,
            bind(db, "UPDATE billing_credit_hold
                      SET reserved=?, settled=?, released=?, expired=?, status=?, updated_at=?, version=version+1
                      WHERE id=? AND version=?",
                 &[number(updated.reserved)?, number(updated.settled)?, number(updated.released)?,
                   number(updated.expired)?, updated.status.into(), number(now)?, self.id.as_str().into(),
                   number(self.version)?])?,
        ])
    }
}

pub(crate) async fn find_hold(
    db: &D1Database,
    scope: &CreditScope<'_>,
    id: &str,
) -> Result<HoldRow> {
    read_hold(db, scope, id)?
        .first(None)
        .await
        .map_err(|_| UNAVAILABLE)?
        .ok_or(ApiError::NOT_FOUND)
}

pub(super) fn read_hold(
    db: &D1Database,
    scope: &CreditScope<'_>,
    id: &str,
) -> Result<D1PreparedStatement> {
    let mut values = scope.values().to_vec();
    values.push(id.into());
    bind(db, "SELECT id, credit_period_id, reference_id, actor_user_id, calling_service,
                    status, amount, reserved, settled, released, expired, expires_at, occurred_at, version
              FROM billing_credit_hold WHERE product=? AND owner_kind=? AND owner_id=? AND id=?",
         &values)
}

impl Write<'_> {
    pub(super) fn open(
        &self,
        input: &HoldRequest,
        period: Option<CreditPeriod>,
    ) -> Result<Mutation> {
        if input.expires_at <= self.now {
            return Err(INVALID);
        }
        let scope = self.usage.scope();
        let period = period.ok_or(INSUFFICIENT)?;
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
            number(input.amount)?,
            number(input.amount)?,
            number(self.usage.occurred_at)?,
            number(self.now)?,
            number(input.expires_at)?,
            number(self.now)?,
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

    pub(super) fn release(&self, row: HoldRow, period: Option<CreditPeriod>) -> Result<Mutation> {
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
            let period = period.ok_or(UNAVAILABLE)?;
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
