use serde::{Deserialize, Serialize};
use worker::{D1Database, D1PreparedStatement};

use crate::core::{
    calendar::Interval,
    http::{ApiError, ApiResult},
    identity::OwnerKind,
    storage::{db_result, prepare, rows},
};

const QUOTES: &str = "SELECT p.plan_id, p.current_version AS plan_version,
    p.product, p.owner_kind, v.name, v.currency, v.monthly_price_minor,
    v.yearly_price_minor, v.monthly_credits, p.sale_status
    FROM billing_plan p JOIN billing_plan_version v
      ON v.plan_id = p.plan_id AND v.plan_version = p.current_version
    WHERE p.product = ? AND p.owner_kind = ?";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct Quote {
    pub(crate) plan_id: String,
    pub(crate) plan_version: i64,
    pub(crate) product: String,
    pub(crate) owner_kind: OwnerKind,
    pub(crate) name: String,
    pub(crate) currency: String,
    pub(crate) monthly_price_minor: i64,
    pub(crate) yearly_price_minor: i64,
    pub(crate) monthly_credits: i64,
    #[serde(skip_serializing)]
    sale_status: String,
}

impl Quote {
    pub(crate) fn amount_minor(&self, interval: Interval) -> i64 {
        match interval {
            Interval::Month => self.monthly_price_minor,
            Interval::Year => self.yearly_price_minor,
        }
    }
}

pub(crate) async fn list_plans(
    db: &D1Database,
    product: &str,
    owner_kind: OwnerKind,
) -> ApiResult<Vec<Quote>> {
    let plans = prepare(
        db,
        &format!("{QUOTES} AND p.sale_status = 'ON_SALE' ORDER BY p.plan_id"),
        &[product.into(), owner_kind.as_str().into()],
    )?;
    rows(&db_result(plans.all().await)?)
}

pub(crate) async fn find_quote(
    db: &D1Database,
    product: &str,
    owner_kind: OwnerKind,
    plan_id: &str,
) -> ApiResult<Quote> {
    let quote = read_quote(db, product, owner_kind, plan_id)?
        .first(None)
        .await
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
    require_quote(quote)
}

pub(crate) fn read_quote(
    db: &D1Database,
    product: &str,
    owner_kind: OwnerKind,
    plan_id: &str,
) -> ApiResult<D1PreparedStatement> {
    prepare(
        db,
        &format!("{QUOTES} AND p.plan_id = ?"),
        &[product.into(), owner_kind.as_str().into(), plan_id.into()],
    )
}

pub(crate) fn require_quote(quote: Option<Quote>) -> ApiResult<Quote> {
    let quote = quote.ok_or(ApiError::NOT_FOUND)?;
    match quote.sale_status.as_str() {
        "ON_SALE" => Ok(quote),
        "DISCONTINUED" => Err(ApiError::PLAN_DISCONTINUED),
        _ => Err(ApiError::BILLING_UNAVAILABLE),
    }
}
