use serde::{Deserialize, Serialize};

use serde_json::{Value, json};
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement};

use crate::{
    catalog::Quote,
    core::{
        calendar::{self, Anchor, Interval, Period},
        http::{ApiError, ApiResult, generated_id as new_id},
        identity::{Owner, VerifiedUser},
        pagination::Page,
        storage::{self, db_result, integer, prepare},
    },
};

use super::service::PAYMENT_WINDOW_MS;

const READ_ORDER: &str = "SELECT o.*, p.starts_at, p.ends_at, p.revoked_at
  FROM billing_order o LEFT JOIN billing_order_service_period p ON p.order_id=o.id";
const SCOPED: &str = "o.product=? AND o.owner_kind=? AND o.owner_id=?";

#[derive(Clone, Copy)]
pub(super) struct Subject<'a> {
    pub(crate) product: &'a str,
    pub(crate) owner: &'a Owner,
}

impl Subject<'_> {
    pub(crate) fn bind(self) -> [JsValue; 3] {
        [
            self.product,
            self.owner.kind.as_str(),
            self.owner.id.as_str(),
        ]
        .map(JsValue::from_str)
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum Status {
    PendingPayment,
    Expired,
    Paid,
    Refunded,
}

#[derive(Deserialize)]
pub(super) struct Order {
    pub(crate) id: String,
    pub(crate) product: String,
    pub(crate) owner_kind: crate::core::identity::OwnerKind,
    pub(crate) owner_id: String,
    pub(crate) actor_user_id: String,
    pub(crate) plan_id: String,
    pub(crate) plan_version: i64,
    pub(crate) interval: Interval,
    pub(crate) currency: String,
    pub(crate) amount_minor: i64,
    pub(crate) monthly_credits: i64,
    pub(crate) billing_time_zone: String,
    pub(crate) status: Status,
    pub(crate) created_at: i64,
    pub(crate) payment_expires_at: i64,
    pub(crate) paid_at: Option<i64>,
    pub(crate) refunded_at: Option<i64>,
    pub(crate) payment_provider: Option<String>,
    pub(crate) provider_payment_id: Option<String>,
    pub(crate) version: i64,
    pub(crate) starts_at: Option<i64>,
    pub(crate) ends_at: Option<i64>,
    pub(crate) revoked_at: Option<i64>,
}

impl Order {
    pub(crate) fn pending(
        subject: Subject<'_>,
        user: &VerifiedUser,
        quote: &Quote,
        interval: Interval,
        zone: &str,
        now: i64,
    ) -> Self {
        Self {
            id: new_id(),
            product: subject.product.into(),
            owner_kind: subject.owner.kind,
            owner_id: subject.owner.id.clone(),
            actor_user_id: user.user_id.clone(),
            plan_id: quote.plan_id.clone(),
            plan_version: quote.plan_version,
            interval,
            currency: quote.currency.clone(),
            amount_minor: quote.amount_minor(interval),
            monthly_credits: quote.monthly_credits,
            billing_time_zone: zone.into(),
            status: Status::PendingPayment,
            created_at: now,
            payment_expires_at: now + PAYMENT_WINDOW_MS,
            paid_at: None,
            refunded_at: None,
            payment_provider: None,
            provider_payment_id: None,
            version: 1,
            starts_at: None,
            ends_at: None,
            revoked_at: None,
        }
    }

    pub(crate) fn owner(&self) -> Owner {
        Owner {
            kind: self.owner_kind,
            id: self.owner_id.clone(),
        }
    }

    pub(crate) fn status_at(&self, now: i64) -> Status {
        if self.status == Status::PendingPayment && now >= self.payment_expires_at {
            Status::Expired
        } else {
            self.status
        }
    }

    pub(crate) fn refundable(&self, now: i64) -> bool {
        self.status == Status::Paid
            && self.revoked_at.is_none()
            && self
                .starts_at
                .is_some_and(|start| calendar::refund_allowed(start, now))
    }

    pub(crate) fn data(&self, now: i64) -> Value {
        let service_period = self.starts_at.zip(self.ends_at).map(|(start, end)| {
            json!({ "orderId": self.id, "startsAt": start, "endsAt": end, "revokedAt": self.revoked_at })
        });
        json!({
            "id": self.id, "product": self.product,
            "ownerKind": self.owner_kind, "ownerId": self.owner_id, "actorUserId": self.actor_user_id,
            "planId": self.plan_id, "planVersion": self.plan_version, "interval": self.interval,
            "currency": self.currency, "amountMinor": self.amount_minor, "monthlyCredits": self.monthly_credits,
            "billingTimeZone": self.billing_time_zone, "status": self.status_at(now),
            "createdAt": self.created_at, "paymentExpiresAt": self.payment_expires_at,
            "paidAt": self.paid_at, "refundedAt": self.refunded_at,
            "servicePeriod": service_period, "refundable": self.refundable(now),
        })
    }

    pub(crate) fn insert(&self, db: &D1Database) -> ApiResult<D1PreparedStatement> {
        prepare(db, "INSERT INTO billing_order
            (id, product, owner_kind, owner_id, actor_user_id, plan_id, plan_version, interval,
             currency, amount_minor, monthly_credits, billing_time_zone, status, created_at, payment_expires_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'PENDING_PAYMENT', ?, ?)", &[
            self.id.as_str().into(), self.product.as_str().into(), self.owner_kind.as_str().into(),
            self.owner_id.as_str().into(), self.actor_user_id.as_str().into(), self.plan_id.as_str().into(),
            integer(self.plan_version)?, self.interval.as_str().into(), self.currency.as_str().into(),
            integer(self.amount_minor)?, integer(self.monthly_credits)?, self.billing_time_zone.as_str().into(),
            integer(self.created_at)?, integer(self.payment_expires_at)?,
        ])
    }

    pub(crate) async fn find(db: &D1Database, subject: Subject<'_>, id: &str) -> ApiResult<Self> {
        db_result(Self::read(db, Some(subject), id)?.first(None).await)?.ok_or(ApiError::NOT_FOUND)
    }

    pub(crate) fn read(
        db: &D1Database,
        subject: Option<Subject<'_>>,
        id: &str,
    ) -> ApiResult<D1PreparedStatement> {
        if let Some(subject) = subject {
            let mut values = subject.bind().to_vec();
            values.push(id.into());
            prepare(
                db,
                &format!("{READ_ORDER} WHERE {SCOPED} AND o.id=?"),
                &values,
            )
        } else {
            prepare(db, &format!("{READ_ORDER} WHERE o.id=?"), &[id.into()])
        }
    }

    pub(crate) async fn list(
        db: &D1Database,
        subject: Subject<'_>,
        page: &Page,
    ) -> ApiResult<Vec<Self>> {
        let mut values = subject.bind().to_vec();
        let cursor = if let Some(cursor) = &page.cursor {
            values.extend([integer(cursor.created_at)?, cursor.id.as_str().into()]);
            " AND (o.created_at, o.id) < (?,?)"
        } else {
            ""
        };
        values.push((page.limit + 1).into());
        let sql = format!(
            "{READ_ORDER} WHERE {SCOPED}{cursor} ORDER BY o.created_at DESC, o.id DESC LIMIT ?"
        );
        db_result(db_result(prepare(db, &sql, &values)?.all().await)?.results())
    }

    pub(crate) async fn pending_expired(
        db: &D1Database,
        now: i64,
        limit: u32,
    ) -> ApiResult<Vec<Self>> {
        let sql = format!(
            "{READ_ORDER} WHERE o.status='PENDING_PAYMENT' AND o.payment_expires_at<=?
            ORDER BY o.payment_expires_at, o.id LIMIT ?"
        );
        db_result(
            db_result(
                prepare(db, &sql, &[integer(now)?, limit.into()])?
                    .all()
                    .await,
            )?
            .results(),
        )
    }
}

#[derive(Deserialize)]
pub(super) struct ServicePeriod {
    pub(crate) order_id: String,
    pub(crate) subscription_id: String,
    pub(crate) month_offset: u32,
    pub(crate) month_count: u32,
    pub(crate) starts_at: i64,
    pub(crate) ends_at: i64,
    pub(crate) anchor_at: i64,
    pub(crate) billing_time_zone: String,
    pub(crate) plan_id: String,
    plan_version: i64,
    plan_name: String,
    interval: Interval,
    monthly_credits: i64,
}

impl ServicePeriod {
    pub(crate) fn anchor(&self) -> Anchor {
        Anchor {
            at: self.anchor_at,
            time_zone: self.billing_time_zone.clone(),
        }
    }

    pub(crate) fn calendar_period(&self) -> Period {
        Period {
            month_offset: self.month_offset,
            month_count: self.month_count,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
        }
    }

    pub(crate) fn data(&self, show_order_id: bool) -> Value {
        let mut value = json!({
            "startsAt": self.starts_at, "endsAt": self.ends_at,
            "planId": self.plan_id, "planVersion": self.plan_version, "planName": self.plan_name,
            "interval": self.interval, "monthlyCredits": self.monthly_credits,
        });
        if show_order_id {
            value["orderId"] = json!(self.order_id);
        }
        value
    }
}

pub(super) struct Timeline {
    pub(crate) current: Option<ServicePeriod>,
    pub(crate) future: Option<ServicePeriod>,
}

impl Timeline {
    pub(crate) async fn load(db: &D1Database, subject: Subject<'_>, now: i64) -> ApiResult<Self> {
        let rows = db_result(Self::read(db, &subject.bind(), now)?.all().await)?;
        Self::from_rows(storage::rows(&rows)?, now)
    }

    pub(crate) fn read(
        db: &D1Database,
        scope: &[JsValue; 3],
        now: i64,
    ) -> ApiResult<D1PreparedStatement> {
        Self::query(
            db,
            "p.product=? AND p.owner_kind=? AND p.owner_id=?",
            scope,
            now,
        )
    }

    pub(crate) fn for_order(
        db: &D1Database,
        order_id: &str,
        now: i64,
    ) -> ApiResult<D1PreparedStatement> {
        Self::query(
            db,
            "(p.product,p.owner_kind,p.owner_id) = (
            SELECT product,owner_kind,owner_id FROM billing_order WHERE id=?)",
            &[order_id.into()],
            now,
        )
    }

    fn query(
        db: &D1Database,
        scope: &str,
        params: &[JsValue],
        now: i64,
    ) -> ApiResult<D1PreparedStatement> {
        let mut values = params.to_vec();
        values.push(integer(now)?);
        let query = format!("SELECT p.order_id, p.subscription_id, p.month_offset, p.month_count, p.starts_at,
               p.ends_at, s.anchor_at, s.billing_time_zone, o.plan_id, o.plan_version, v.name AS plan_name,
               o.interval, o.monthly_credits
            FROM billing_order_service_period p
            JOIN billing_subscription s ON s.id=p.subscription_id
            JOIN billing_order o ON o.id=p.order_id
            JOIN billing_plan_version v ON v.plan_id=o.plan_id AND v.plan_version=o.plan_version
            WHERE {scope} AND p.ends_at>? AND p.revoked_at IS NULL AND o.status='PAID'
            ORDER BY p.starts_at LIMIT 3");
        prepare(db, &query, &values)
    }

    pub(crate) fn from_rows(mut rows: Vec<ServicePeriod>, now: i64) -> ApiResult<Self> {
        let future = if rows.last().is_some_and(|p| p.starts_at > now) {
            rows.pop()
        } else {
            None
        };
        let current = rows.pop();
        if !rows.is_empty()
            || current
                .as_ref()
                .is_some_and(|p| !calendar::covers(p.starts_at, p.ends_at, now))
        {
            return Err(ApiError::BILLING_UNAVAILABLE);
        }
        Ok(Self { current, future })
    }

    pub(crate) fn require_plan(&self, plan_id: &str) -> ApiResult<()> {
        if self
            .current
            .as_ref()
            .or(self.future.as_ref())
            .is_some_and(|p| p.plan_id != plan_id)
        {
            return Err(ApiError::VERSION_CONFLICT);
        }
        Ok(())
    }
}
