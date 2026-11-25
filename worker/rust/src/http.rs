use serde::Serialize;
use serde_json::json;
use wasm_bindgen::prelude::*;
use worker::{Request, Response, Result};

#[wasm_bindgen(module = "/src/runtime/foundation.js")]
extern "C" {
    #[wasm_bindgen(js_name = requestId)]
    fn new_request_id() -> String;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: &'static str,
}

impl ApiError {
    pub const INVALID_REQUEST: Self = Self::new(400, "INVALID_REQUEST", "Invalid request");
    pub const INVALID_TIMEZONE: Self =
        Self::new(400, "INVALID_TIMEZONE", "Invalid billing time zone");
    pub const UNAUTHENTICATED: Self = Self::new(401, "UNAUTHENTICATED", "Authentication required");
    pub const SESSION_EXPIRED: Self = Self::new(401, "SESSION_EXPIRED", "Session expired");
    pub const INSUFFICIENT_CREDITS: Self =
        Self::new(402, "INSUFFICIENT_CREDITS", "Insufficient credits");
    pub const FORBIDDEN: Self = Self::new(403, "FORBIDDEN", "Access denied");
    pub const TENANT_DISABLED: Self = Self::new(403, "TENANT_DISABLED", "Tenant disabled");
    pub const NOT_FOUND: Self = Self::new(404, "NOT_FOUND", "Resource not found");
    pub const ORDER_EXPIRED: Self = Self::new(409, "ORDER_EXPIRED", "Order expired");
    pub const RENEWAL_ALREADY_PURCHASED: Self = Self::new(
        409,
        "RENEWAL_ALREADY_PURCHASED",
        "Renewal already purchased",
    );
    pub const ORDER_NOT_REFUNDABLE: Self =
        Self::new(409, "ORDER_NOT_REFUNDABLE", "Order is not refundable");
    pub const PLAN_DISCONTINUED: Self = Self::new(409, "PLAN_DISCONTINUED", "Plan discontinued");
    pub const IDEMPOTENCY_CONFLICT: Self =
        Self::new(409, "IDEMPOTENCY_CONFLICT", "Idempotency key already used");
    pub const VERSION_CONFLICT: Self = Self::new(409, "VERSION_CONFLICT", "Concurrent update");
    pub const AUTH_UNAVAILABLE: Self =
        Self::new(503, "AUTH_UNAVAILABLE", "Identity service unavailable");
    pub const BILLING_UNAVAILABLE: Self =
        Self::new(503, "BILLING_UNAVAILABLE", "Billing service unavailable");

    pub const fn new(status: u16, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    pub fn response(self) -> Result<Response> {
        error(self.status, self.code, self.message)
    }
}

pub fn request_id(request: &Request) -> String {
    request
        .headers()
        .get("X-Request-Id")
        .ok()
        .flatten()
        .filter(|value| valid_id(value))
        .unwrap_or_else(new_request_id)
}

pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub fn json_response(status: u16, body: &impl Serialize) -> Result<Response> {
    let mut response = Response::from_json(body)?.with_status(status);
    let headers = response.headers_mut();
    headers.set("Content-Type", "application/json; charset=utf-8")?;
    headers.set("Cache-Control", "no-store")?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    headers.set("X-Frame-Options", "DENY")?;
    headers.set("Referrer-Policy", "no-referrer")?;
    Ok(response)
}

pub fn error(status: u16, code: &str, message: &str) -> Result<Response> {
    json_response(
        status,
        &json!({ "error": { "code": code, "message": message } }),
    )
}
