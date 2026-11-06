mod access;
mod runtime;
pub(crate) use access::{HumanGuardState, SecretsAccessAdapter, human_guard};
pub use access::{initialize_human_capability, initialize_tls_proxy_capability, tls_proxy_configured};
pub(crate) use runtime::{SecretsRuntimeState, runtime_routes};

pub(crate) use access::SecretsDisableObserver;

#[cfg(test)]
mod access_tests;
