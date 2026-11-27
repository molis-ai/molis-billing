//! Identity comes from a fresh server-to-server Auth assertion for the specific subject.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;
use worker::{Env, Request};

use crate::http::{ApiError, valid_id};

const INTERNAL_HEADER: &str = "X-Molis-Internal-Secret";

#[wasm_bindgen(module = "/src/runtime/foundation.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = authFetch)]
    async fn auth_fetch(
        env: &JsValue,
        path: &str,
        authorization: &str,
        body: &str,
        request_id: &str,
    ) -> Result<String, JsValue>;

    #[wasm_bindgen(catch, js_name = matchSecret)]
    async fn match_secret(expected: &str, provided: &str) -> Result<bool, JsValue>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OwnerKind {
    User,
    Tenant,
}

impl OwnerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "USER",
            Self::Tenant => "TENANT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Owner {
    pub kind: OwnerKind,
    pub id: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Role {
    #[serde(rename = "SELF")]
    Personal,
    Owner,
    Admin,
    Member,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedUser {
    pub user_id: String,
    pub product: String,
    pub owner: Owner,
    pub role: Role,
}

impl VerifiedUser {
    pub fn ledger_actor_filter(&self) -> Option<&str> {
        (self.role == Role::Member).then_some(&self.user_id)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UserAccess {
    Read,
    Manage,
}

#[derive(Debug)]
pub struct UserContext {
    pub product: String,
    pub owner_kind: OwnerKind,
    pub owner_id: Option<String>,
}

impl UserContext {
    pub fn from_request(request: &Request, paginated: bool) -> Result<Self, ApiError> {
        let url = request.url().map_err(|_| ApiError::INVALID_REQUEST)?;
        let mut product = None;
        let mut kind = None;
        let mut id = None;
        for (key, value) in url.query_pairs() {
            let slot = match key.as_ref() {
                "product" => &mut product,
                "ownerKind" => &mut kind,
                "ownerId" => &mut id,
                "limit" | "cursor" if paginated => continue,
                _ => return Err(ApiError::INVALID_REQUEST),
            };
            if slot.replace(value.into_owned()).is_some() {
                return Err(ApiError::INVALID_REQUEST);
            }
        }
        let owner_kind = match kind.as_deref() {
            None | Some("USER") => OwnerKind::User,
            Some("TENANT") => OwnerKind::Tenant,
            _ => return Err(ApiError::INVALID_REQUEST),
        };
        if id.as_ref().is_some_and(|value| !valid_id(value))
            || (owner_kind == OwnerKind::Tenant && id.is_none())
            || product.as_deref().is_some_and(|value| value != "WORK")
        {
            return Err(ApiError::INVALID_REQUEST);
        }
        Ok(Self {
            product: product.unwrap_or_else(|| "WORK".into()),
            owner_kind,
            owner_id: id,
        })
    }
}

pub async fn require_internal_channel(request: &Request, env: &Env) -> Result<(), ApiError> {
    let secret = env
        .secret("MOLIS_INTERNAL_SECRET")
        .ok()
        .map(|value| value.to_string())
        .filter(|value| valid_secret(value))
        .ok_or(ApiError::BILLING_UNAVAILABLE)?;
    let candidate = request
        .headers()
        .get(INTERNAL_HEADER)
        .ok()
        .flatten()
        .filter(|value| valid_secret(value))
        .ok_or(ApiError::NOT_FOUND)?;
    if !match_secret(&secret, &candidate)
        .await
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
    {
        return Err(ApiError::NOT_FOUND);
    }
    Ok(())
}

fn valid_secret(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub async fn authorize_user(
    request: &Request,
    env: &Env,
    subject: UserContext,
    access: UserAccess,
    request_id: &str,
) -> Result<VerifiedUser, ApiError> {
    let mut owner = json!({ "kind": subject.owner_kind });
    if let Some(id) = &subject.owner_id {
        owner["id"] = json!(id);
    }
    let data = call_auth(
        request,
        env,
        "/internal/v1/subjects/verify",
        json!({ "product": subject.product, "owner": owner }),
        request_id,
    )
    .await?;
    let verified: VerifiedUser =
        serde_json::from_value(data).map_err(|_| ApiError::AUTH_UNAVAILABLE)?;
    if !valid_id(&verified.user_id)
        || !valid_id(&verified.owner.id)
        || verified.product != subject.product
        || verified.owner.kind != subject.owner_kind
        || subject.owner_id.is_some_and(|id| id != verified.owner.id)
        || (verified.owner.kind == OwnerKind::User
            && (verified.role != Role::Personal || verified.owner.id != verified.user_id))
        || (verified.owner.kind == OwnerKind::Tenant && verified.role == Role::Personal)
    {
        return Err(ApiError::AUTH_UNAVAILABLE);
    }
    if access == UserAccess::Manage && verified.role == Role::Member {
        return Err(ApiError::FORBIDDEN);
    }
    Ok(verified)
}

pub async fn introspect_session(
    request: &Request,
    env: &Env,
    request_id: &str,
) -> Result<String, ApiError> {
    let data = call_auth(
        request,
        env,
        "/internal/v1/sessions/introspect",
        json!({}),
        request_id,
    )
    .await?;
    data["userId"]
        .as_str()
        .filter(|id| valid_id(id))
        .map(str::to_owned)
        .ok_or(ApiError::AUTH_UNAVAILABLE)
}

pub(crate) async fn authorize_platform_audit(
    request: &Request,
    env: &Env,
    request_id: &str,
) -> Result<(), ApiError> {
    let data = call_auth(
        request,
        env,
        "/internal/v1/platform/audit/authorize",
        json!({}),
        request_id,
    )
    .await
    .map_err(|failure| match failure {
        // A missing private authorization route is an unavailable dependency.
        ApiError::NOT_FOUND => ApiError::AUTH_UNAVAILABLE,
        failure => failure,
    })?;
    if !data["userId"].as_str().is_some_and(valid_id) || data["permission"] != "AUDIT_READ" {
        return Err(ApiError::AUTH_UNAVAILABLE);
    }
    Ok(())
}

async fn call_auth(
    request: &Request,
    env: &Env,
    path: &str,
    body: Value,
    request_id: &str,
) -> Result<Value, ApiError> {
    let authorization = request
        .headers()
        .get("Authorization")
        .ok()
        .flatten()
        .filter(|value| {
            value.strip_prefix("Bearer ").is_some_and(|token| {
                !token.is_empty() && !token.bytes().any(|byte| byte.is_ascii_whitespace())
            })
        })
        .ok_or(ApiError::UNAUTHENTICATED)?;

    let raw = auth_fetch(
        env.as_ref(),
        path,
        &authorization,
        &body.to_string(),
        request_id,
    )
    .await
    .map_err(|_| ApiError::AUTH_UNAVAILABLE)?;
    let mut response: AuthResponse =
        serde_json::from_str(&raw).map_err(|_| ApiError::AUTH_UNAVAILABLE)?;
    match (response.status, response.result["error"]["code"].as_str()) {
        (200, None) if response.result["data"].is_object() => Ok(response.result["data"].take()),
        (401, Some("UNAUTHENTICATED")) => Err(ApiError::UNAUTHENTICATED),
        (401, Some("SESSION_EXPIRED" | "SESSION_REVOKED")) => Err(ApiError::SESSION_EXPIRED),
        (403, Some("TENANT_DISABLED")) => Err(ApiError::TENANT_DISABLED),
        (403, Some("FORBIDDEN")) => Err(ApiError::FORBIDDEN),
        (404, Some("NOT_FOUND")) => Err(ApiError::NOT_FOUND),
        _ => Err(ApiError::AUTH_UNAVAILABLE),
    }
}

#[derive(Deserialize)]
struct AuthResponse {
    status: u16,
    result: Value,
}
