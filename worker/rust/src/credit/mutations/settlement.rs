use crate::core::{
    http::{ApiError, ApiResult},
    storage::{integer, prepare},
};

use super::super::model::{CreditPeriod, Delta};
use super::{
    holds::{HoldRow, HoldStatus},
    runner::Write,
    types::{INSUFFICIENT, Mutation, Segment, update_period},
};

impl Write<'_> {
    pub(super) async fn consume(
        &self,
        at: i64,
        amount: i64,
        period: Option<CreditPeriod>,
    ) -> ApiResult<Mutation> {
        let period = period.ok_or(INSUFFICIENT)?;
        let mut delta = Delta {
            consumed: amount,
            ..Delta::default()
        };
        if period.status == "EXPIRED" {
            let available = prepare(
                self.db,
                include_str!("../sql/consume_expired.sql"),
                &[period.id.as_str().into(), integer(period.version)?],
            )?
            .first::<i64>(Some("available"))
            .await
            .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
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

    pub(super) async fn settle_one(
        &self,
        row: HoldRow,
        segment: Segment,
        held_period: Option<CreditPeriod>,
        segment_period: Option<CreditPeriod>,
    ) -> ApiResult<Mutation> {
        let period = held_period.ok_or(INSUFFICIENT)?;
        if period.id != row.credit_period_id {
            return Err(ApiError::BILLING_UNAVAILABLE);
        }
        if segment.occurred_at >= period.expires_at {
            if row.expires_at < period.expires_at {
                return Err(INSUFFICIENT);
            }
            let mut mutation = self
                .consume(segment.occurred_at, segment.amount, segment_period)
                .await?;
            // The newer period cannot use the old hold FK. The audit and reference link the task.
            mutation.hold = Some(row.snapshot());
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
}
