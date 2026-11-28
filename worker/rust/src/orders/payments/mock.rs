use hmac::{Hmac, Mac};
use sha2::Sha256;
use worker::{Env, Request};

use crate::core::http::{ApiError, ApiResult};

use super::processor::{Event, Kind, PaymentProvider, Verification};

const NAME: &str = "mock";

fn verify_signature(secret: &str, body: &[u8], header: &str) -> bool {
    if header.len() != 64 {
        return false;
    }
    let digit = |byte| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut provided = [0; 32];
    for (pair, output) in header.as_bytes().chunks_exact(2).zip(&mut provided) {
        let (Some(high), Some(low)) = (digit(pair[0]), digit(pair[1])) else {
            return false;
        };
        *output = high << 4 | low;
    }
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

pub(super) struct Adapter;

pub(super) fn enabled(env: &Env) -> bool {
    env.var("MOLIS_ENVIRONMENT")
        .is_ok_and(|environment| matches!(environment.to_string().as_str(), "dev" | "test"))
}

impl PaymentProvider for Adapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn identifiers(&self, id: &str, kind: Kind) -> (String, String) {
        let operation = match kind {
            Kind::Payment => "pay",
            Kind::Refund => "refund",
        };
        (format!("{operation}.{id}"), payment_id(id))
    }

    fn verify<'a>(&self, request: &'a mut Request, env: &'a Env) -> Verification<'a> {
        Box::pin(verify_request(request, env))
    }

    fn accepts(&self, event: &Event) -> bool {
        event.payment_id == payment_id(&event.order_id)
    }
}

async fn verify_request(request: &mut Request, env: &Env) -> ApiResult<Event> {
    let secret = env
        .secret("MOLIS_MOCK_WEBHOOK_SECRET")
        .ok()
        .map(|value| value.to_string())
        .filter(|value| value.len() >= 32)
        .ok_or(ApiError::BILLING_UNAVAILABLE)?;
    let signature = request
        .headers()
        .get("X-Molis-Payment-Signature")
        .ok()
        .flatten()
        .ok_or(ApiError::FORBIDDEN)?;
    let bytes = request
        .bytes()
        .await
        .map_err(|_| ApiError::INVALID_REQUEST)?;
    if bytes.len() > 4096 {
        return Err(ApiError::INVALID_REQUEST);
    }
    if !verify_signature(&secret, &bytes, &signature) {
        return Err(ApiError::FORBIDDEN);
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::INVALID_REQUEST)
}

fn payment_id(order_id: &str) -> String {
    format!("mock.{order_id}")
}
