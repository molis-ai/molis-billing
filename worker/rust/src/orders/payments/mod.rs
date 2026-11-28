//! Payment providers, fulfillment, and webhook processing.

#[cfg(feature = "mock-payment")]
mod mock;
mod mutations;
mod processor;
mod records;

pub(crate) use processor::webhook;
