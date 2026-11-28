use wasm_bindgen::JsValue;
use worker::D1PreparedStatement;

use crate::core::{
    calendar::{self, Anchor, Period},
    http::{ApiError, ApiResult, generated_id as new_id},
    storage::{integer, prepare},
};

use super::super::{
    model::{Order, ServicePeriod, Status, Subject},
    service::Orders,
};
use super::records::PaymentEvent;

impl Orders<'_> {
    pub(super) fn stage_payment(
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
            integer(now)?,
            order.id.as_str().into(),
            integer(order.version)?,
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
                integer(now)?,
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
                    integer(anchor.at)?,
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
                integer(period.starts_at)?,
                integer(period.ends_at)?,
            ],
        )?);
        order.status = Status::Paid;
        order.paid_at = Some(now);
        order.starts_at = Some(period.starts_at);
        order.ends_at = Some(period.ends_at);
        Ok((period.starts_at == now).then_some((anchor, period)))
    }

    pub(super) fn stage_refund(
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
            &[integer(now)?, order.id.as_str().into(), integer(order.version)?],
        )?);
        batch.push(prepare(
            self.db,
            "UPDATE billing_order
            SET status='REFUNDED', refunded_at=?, version=version+1 WHERE id=?",
            &[integer(now)?, order.id.as_str().into()],
        )?);
        batch.push(prepare(
            self.db,
            "UPDATE billing_order_service_period
            SET revoked_at=?, version=version+1 WHERE order_id=?",
            &[integer(now)?, order.id.as_str().into()],
        )?);
        order.status = Status::Refunded;
        order.refunded_at = Some(now);
        order.revoked_at = Some(now);
        Ok(())
    }
}
