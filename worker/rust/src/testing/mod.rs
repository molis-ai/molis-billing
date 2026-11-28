//! Local test routes and fault injection.

mod credit_fixture;
mod hooks;

pub(crate) use hooks::{Hooks, dispatch};
