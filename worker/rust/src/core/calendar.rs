//! All billing boundaries use UTC milliseconds and the original local calendar anchor.
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

use crate::core::http::{ApiError, ApiResult};

const DEFAULT_ZONE: &str = "Asia/Shanghai";
pub(crate) const DATE_MAX: i64 = 8_640_000_000_000_000;

#[wasm_bindgen(module = "/src/runtime/foundation.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = normalizeZone)]
    fn normalize_zone(zone: &str) -> Result<String, JsValue>;
    #[wasm_bindgen(catch, js_name = monthBoundary)]
    fn month_boundary(anchor_at: f64, zone: &str, month_offset: u32) -> Result<f64, JsValue>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum Interval {
    Month,
    Year,
}

impl Interval {
    pub(crate) const fn months(self) -> u32 {
        match self {
            Self::Month => 1,
            Self::Year => 12,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Month => "MONTH",
            Self::Year => "YEAR",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub(crate) at: i64,
    pub(crate) time_zone: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Period {
    pub(crate) month_offset: u32,
    pub(crate) month_count: u32,
    pub(crate) starts_at: i64,
    pub(crate) ends_at: i64,
}

/// Only pass a current, non-revoked, same-plan period from the authenticated subject.
pub(crate) struct Renewal<'a> {
    pub(crate) anchor: &'a Anchor,
    pub(crate) current: Period,
}

pub(crate) fn validate_time_zone(raw: Option<&str>) -> ApiResult<String> {
    normalize_zone(raw.unwrap_or(DEFAULT_ZONE)).map_err(|_| ApiError::INVALID_TIMEZONE)
}

pub(crate) fn boundary_at(anchor: &Anchor, month_offset: u32) -> ApiResult<i64> {
    if !(0..=DATE_MAX).contains(&anchor.at) {
        return Err(ApiError::BILLING_UNAVAILABLE);
    }
    let value = month_boundary(anchor.at as f64, &anchor.time_zone, month_offset)
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
    if !value.is_finite() || !(0.0..=DATE_MAX as f64).contains(&value) {
        return Err(ApiError::BILLING_UNAVAILABLE);
    }
    Ok(value as i64)
}

pub(crate) const fn covers(starts_at: i64, ends_at: i64, now: i64) -> bool {
    starts_at <= now && now < ends_at
}

pub(crate) const fn refund_allowed(starts_at: i64, now: i64) -> bool {
    now < starts_at
}

/// The commit guard must independently verify the selected current period and future-slot availability.
pub(crate) fn purchase_period(
    paid_at: i64,
    order_time_zone: &str,
    interval: Interval,
    renewal: Option<Renewal<'_>>,
) -> ApiResult<(Anchor, Period)> {
    purchase_period_with(
        paid_at,
        order_time_zone,
        interval,
        renewal,
        validate_time_zone,
        boundary_at,
    )
}

fn purchase_period_with(
    paid_at: i64,
    order_time_zone: &str,
    interval: Interval,
    renewal: Option<Renewal<'_>>,
    validate_zone: impl FnOnce(Option<&str>) -> ApiResult<String>,
    boundary: impl FnOnce(&Anchor, u32) -> ApiResult<i64>,
) -> ApiResult<(Anchor, Period)> {
    let (anchor, month_offset, starts_at) = match renewal {
        Some(renewal) if covers(renewal.current.starts_at, renewal.current.ends_at, paid_at) => {
            let offset = renewal
                .current
                .month_offset
                .checked_add(renewal.current.month_count)
                .ok_or(ApiError::BILLING_UNAVAILABLE)?;
            (renewal.anchor.clone(), offset, renewal.current.ends_at)
        }
        _ => (
            Anchor {
                at: paid_at,
                time_zone: validate_zone(Some(order_time_zone))?,
            },
            0,
            paid_at,
        ),
    };
    let ends_at = boundary(
        &anchor,
        month_offset
            .checked_add(interval.months())
            .ok_or(ApiError::BILLING_UNAVAILABLE)?,
    )?;
    if starts_at >= ends_at {
        return Err(ApiError::BILLING_UNAVAILABLE);
    }
    Ok((
        anchor,
        Period {
            month_offset,
            month_count: interval.months(),
            starts_at,
            ends_at,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a06_interval_changes_use_original_anchor_and_start_after_current_period() {
        let original = Anchor {
            at: 1000,
            time_zone: "Asia/Shanghai".into(),
        };
        for (current_months, next_interval, expected_end_offset) in
            [(1, Interval::Year, 13), (12, Interval::Month, 13)]
        {
            let (anchor, next) = purchase_period_with(
                1999,
                "America/New_York",
                next_interval,
                Some(Renewal {
                    anchor: &original,
                    current: Period {
                        month_offset: 0,
                        month_count: current_months,
                        starts_at: 1000,
                        ends_at: 2000,
                    },
                }),
                |_| panic!("active renewal should not replace its time zone"),
                |anchor, offset| {
                    assert_eq!(anchor, &original);
                    assert_eq!(offset, expected_end_offset);
                    Ok(3000)
                },
            )
            .expect("valid renewal");
            assert_eq!(anchor, original);
            assert_eq!(
                next,
                Period {
                    month_offset: current_months,
                    month_count: next_interval.months(),
                    starts_at: 2000,
                    ends_at: 3000,
                }
            );
            assert!(refund_allowed(next.starts_at, 1999));
            assert!(!refund_allowed(next.starts_at, 2000));
        }
    }

    #[test]
    fn a06_expired_purchase_starts_at_payment_with_a_new_anchor() {
        let original = Anchor {
            at: 1000,
            time_zone: "Asia/Shanghai".into(),
        };
        let (anchor, period) = purchase_period_with(
            2000,
            "America/New_York",
            Interval::Month,
            Some(Renewal {
                anchor: &original,
                current: Period {
                    month_offset: 0,
                    month_count: 1,
                    starts_at: 1000,
                    ends_at: 2000,
                },
            }),
            |zone| Ok(zone.expect("order time zone").into()),
            |anchor, offset| {
                assert_eq!(anchor.at, 2000);
                assert_eq!(anchor.time_zone, "America/New_York");
                assert_eq!(offset, 1);
                Ok(3000)
            },
        )
        .expect("valid new purchase");
        assert_eq!(anchor.at, 2000);
        assert_eq!(period.month_offset, 0);
        assert_eq!(period.starts_at, 2000);
        assert!(!covers(1000, 2000, 2000));
        assert!(covers(period.starts_at, period.ends_at, 2000));
        assert_eq!(ApiError::INVALID_TIMEZONE.status, 400);
    }
}
