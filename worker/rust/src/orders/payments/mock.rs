use super::*;

const NAME: &str = "mock";

#[wasm_bindgen(module = "/src/runtime/mock-payment.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = verifyMockSignature)]
    async fn verify_signature(secret: &str, body: &[u8], header: &str) -> Result<bool, JsValue>;
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

    fn confirm(&self, order: &Order, kind: Kind) -> Event {
        let operation = match kind {
            Kind::Payment => "pay",
            Kind::Refund => "refund",
        };
        Event {
            event_id: format!("{operation}.{}", order.id),
            kind,
            payment_id: payment_id(&order.id),
            order_id: order.id.clone(),
            amount_minor: order.amount_minor,
            currency: order.currency.clone(),
        }
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
    if !verify_signature(&secret, &bytes, &signature)
        .await
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
    {
        return Err(ApiError::FORBIDDEN);
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::INVALID_REQUEST)
}

fn payment_id(order_id: &str) -> String {
    format!("mock.{order_id}")
}
