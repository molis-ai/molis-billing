//! Order reads, writes, and payment integration.

mod model;
mod payments;
mod service;

pub(crate) use payments::webhook;
pub(crate) use service::Orders;
