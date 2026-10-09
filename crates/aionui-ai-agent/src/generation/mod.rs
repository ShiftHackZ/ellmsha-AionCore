//! Request-scoped generation for TencentDB using the selected primary agent.
mod cli;
mod native;
mod service;
mod types;

pub use service::GenerationService;
pub(crate) use service::GenerationTarget;
pub use types::*;
