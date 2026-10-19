//! Request-scoped generation for TencentDB using the selected primary agent.
mod cli;
mod native;
mod service;
mod stateless;
mod types;

pub use service::GenerationService;
pub(crate) use service::GenerationTarget;
pub(crate) use stateless::complete_stateless;
pub use types::*;

pub(crate) use native::complete as complete_native;
