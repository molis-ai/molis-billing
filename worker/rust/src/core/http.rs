use futures_util::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use wasm_bindgen::prelude::wasm_bindgen;
use worker::{Request, Response, Result};

const MAX_BODY: usize = 16 * 1024;

pub(crate) type ApiResult<T> = std::result::Result<T, ApiError>;

#[wasm_bindgen(module = "/src/runtime/foundation.js")]
extern "C" {
    #[wasm_bindgen(js_name = requestId)]
    fn new_request_id() -> String;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub(crate) status: u16,
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
    data: Option<Box<Value>>,
}

impl ApiError {
    pub(crate) const INVALID_REQUEST: Self = Self::new(400, "INVALID_REQUEST", "Invalid request");
    pub(crate) const INVALID_TIMEZONE: Self =
        Self::new(400, "INVALID_TIMEZONE", "Invalid billing time zone");
    pub(crate) const UNAUTHENTICATED: Self =
        Self::new(401, "UNAUTHENTICATED", "Authentication required");
    pub(crate) const SESSION_EXPIRED: Self = Self::new(401, "SESSION_EXPIRED", "Session expired");
    pub(crate) const INSUFFICIENT_CREDITS: Self =
        Self::new(402, "INSUFFICIENT_CREDITS", "Insufficient credits");
    pub(crate) const FORBIDDEN: Self = Self::new(403, "FORBIDDEN", "Access denied");
    pub(crate) const TENANT_DISABLED: Self = Self::new(403, "TENANT_DISABLED", "Tenant disabled");
    pub(crate) const NOT_FOUND: Self = Self::new(404, "NOT_FOUND", "Resource not found");
    pub(crate) const ORDER_EXPIRED: Self = Self::new(409, "ORDER_EXPIRED", "Order expired");
    pub(crate) const RENEWAL_ALREADY_PURCHASED: Self = Self::new(
        409,
        "RENEWAL_ALREADY_PURCHASED",
        "Renewal already purchased",
    );
    pub(crate) const ORDER_NOT_REFUNDABLE: Self =
        Self::new(409, "ORDER_NOT_REFUNDABLE", "Order is not refundable");
    pub(crate) const PLAN_DISCONTINUED: Self =
        Self::new(409, "PLAN_DISCONTINUED", "Plan discontinued");
    pub(crate) const IDEMPOTENCY_CONFLICT: Self =
        Self::new(409, "IDEMPOTENCY_CONFLICT", "Idempotency key already used");
    pub(crate) const VERSION_CONFLICT: Self =
        Self::new(409, "VERSION_CONFLICT", "Concurrent update");
    pub(crate) const AUTH_UNAVAILABLE: Self =
        Self::new(503, "AUTH_UNAVAILABLE", "Identity service unavailable");
    pub(crate) const BILLING_UNAVAILABLE: Self =
        Self::new(503, "BILLING_UNAVAILABLE", "Billing service unavailable");

    pub(crate) const fn new(status: u16, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            data: None,
        }
    }

    pub(crate) fn with_data(mut self, data: Value) -> Self {
        self.data = Some(Box::new(data));
        self
    }

    pub(crate) fn body(self, request_id: &str) -> Value {
        let mut body = json!({
            "error": {"code": self.code, "message": self.message},
            "requestId": request_id
        });
        if let Some(data) = self.data {
            body["data"] = *data;
        }
        body
    }
}

pub(crate) struct ApiReply {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

impl ApiReply {
    pub(crate) fn new(status: u16, data: Value) -> Self {
        Self {
            status,
            body: json!({"data": data}),
        }
    }

    pub(crate) fn ok(data: Value) -> Self {
        Self::new(200, data)
    }

    pub(crate) fn stored(status: u16, body: &str) -> ApiResult<Self> {
        let body = serde_json::from_str(body).map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
        Ok(Self { status, body })
    }
}

pub(crate) fn request_id(request: &Request) -> String {
    request
        .headers()
        .get("X-Request-Id")
        .ok()
        .flatten()
        .filter(|value| valid_id(value))
        .unwrap_or_else(generated_id)
}

pub(crate) fn generated_id() -> String {
    new_request_id()
}

pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) async fn read_json<T: DeserializeOwned>(request: &mut Request) -> ApiResult<Option<T>> {
    if request.inner().body().is_none() {
        return Ok(None);
    }
    let mut body = Vec::new();
    let mut stream = request.stream().map_err(|_| ApiError::INVALID_REQUEST)?;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::INVALID_REQUEST)?;
        if chunk.len() > MAX_BODY - body.len() {
            return Err(ApiError::INVALID_REQUEST);
        }
        body.extend(chunk);
    }
    if body.is_empty() {
        return Ok(None);
    }
    let content_type = request
        .headers()
        .get("Content-Type")
        .map_err(|_| ApiError::INVALID_REQUEST)?;
    if !content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(ApiError::INVALID_REQUEST);
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|_| ApiError::INVALID_REQUEST)
}

pub(crate) fn json_response(
    status: u16,
    body: &impl Serialize,
    request_id: &str,
) -> Result<Response> {
    let mut response = Response::from_json(body)?.with_status(status);
    let headers = response.headers_mut();
    headers.set("Content-Type", "application/json; charset=utf-8")?;
    headers.set("X-Request-Id", request_id)?;
    headers.set("Cache-Control", "no-store")?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    headers.set("X-Frame-Options", "DENY")?;
    headers.set("Referrer-Policy", "no-referrer")?;
    Ok(response)
}
