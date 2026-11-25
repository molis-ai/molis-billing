use serde::{Deserialize, Serialize};
use worker::D1Database;
use worker::d1::D1Type;

use crate::{calendar::Interval, http::ApiError, identity::OwnerKind};

const QUOTES: &str = "SELECT p.plan_id, p.current_version AS plan_version,
    p.product, p.owner_kind, v.name, v.currency, v.monthly_price_minor,
    v.yearly_price_minor, v.monthly_credits, p.sale_status
    FROM billing_plan p JOIN billing_plan_version v
      ON v.plan_id = p.plan_id AND v.plan_version = p.current_version
    WHERE p.product = ? AND p.owner_kind = ?";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    #[serde(alias = "plan_id")]
    pub plan_id: String,
    #[serde(alias = "plan_version")]
    pub plan_version: i64,
    pub product: String,
    #[serde(alias = "owner_kind")]
    pub owner_kind: OwnerKind,
    pub name: String,
    pub currency: String,
    #[serde(alias = "monthly_price_minor")]
    pub monthly_price_minor: i64,
    #[serde(alias = "yearly_price_minor")]
    pub yearly_price_minor: i64,
    #[serde(alias = "monthly_credits")]
    pub monthly_credits: i64,
    #[serde(alias = "sale_status", skip_serializing)]
    sale_status: String,
}

impl Quote {
    pub fn amount_minor(&self, interval: Interval) -> i64 {
        match interval {
            Interval::Month => self.monthly_price_minor,
            Interval::Year => self.yearly_price_minor,
        }
    }
}

pub async fn list_plans(
    db: &D1Database,
    product: &str,
    owner_kind: OwnerKind,
) -> Result<Vec<Quote>, ApiError> {
    db.prepare(format!(
        "{QUOTES} AND p.sale_status = 'ON_SALE' ORDER BY p.plan_id"
    ))
    .bind_refs(&[D1Type::Text(product), D1Type::Text(owner_kind.as_str())])
    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
    .all()
    .await
    .and_then(|rows| rows.results())
    .map_err(|_| ApiError::BILLING_UNAVAILABLE)
}

pub async fn find_quote(
    db: &D1Database,
    product: &str,
    owner_kind: OwnerKind,
    plan_id: &str,
) -> Result<Quote, ApiError> {
    let quote: Quote = db
        .prepare(format!("{QUOTES} AND p.plan_id = ?"))
        .bind_refs(&[
            D1Type::Text(product),
            D1Type::Text(owner_kind.as_str()),
            D1Type::Text(plan_id),
        ])
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
        .first(None)
        .await
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
        .ok_or(ApiError::NOT_FOUND)?;
    match quote.sale_status.as_str() {
        "ON_SALE" => Ok(quote),
        "DISCONTINUED" => Err(ApiError::PLAN_DISCONTINUED),
        _ => Err(ApiError::BILLING_UNAVAILABLE),
    }
}
