//! HTTP router assembly for the application.

mod antigravity_hook;
mod fs_monitor;
mod health;
mod item_revealer;
mod memory_artifact;
mod memory_generation;
mod routes;
mod runtime_router_tools;
mod runtime_team_tools;
mod state;
mod team_conversation_adapters;
mod trace;

pub use routes::{
    RouterRuntime, create_router, create_router_with_all_state, create_router_with_runtime, create_router_with_states,
};
pub use state::{
    ChannelOrchestratorComponents, ModuleStates, RouterBuildError, build_assistant_state, build_conversation_state,
    build_extension_states, build_module_states, build_ws_state,
};
