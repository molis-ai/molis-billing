use serde_json::{Value, json};
use worker::Env;

pub(crate) fn environment(env: &Env) -> &'static str {
    match env
        .var("MOLIS_ENVIRONMENT")
        .ok()
        .map(|value| value.to_string())
        .as_deref()
    {
        Some("prod") => "prod",
        Some("test") => "test",
        Some("dev") => "dev",
        _ => "unknown",
    }
}

pub(crate) fn event<const N: usize>(
    name: &'static str,
    env: &'static str,
    request_id: &str,
    fields: [(&'static str, Value); N],
    error: bool,
) {
    let envelope = [
        ("schema", json!(1)),
        ("service", json!("molis-billing")),
        ("env", json!(env)),
        ("event", json!(name)),
        ("request_id", json!(request_id)),
    ];
    let message = Value::Object(
        fields
            .into_iter()
            .chain(envelope)
            .map(|(name, value)| (name.into(), value))
            .collect(),
    );
    if error {
        worker::console_error!("{message}");
    } else {
        worker::console_log!("{message}");
    }
}
