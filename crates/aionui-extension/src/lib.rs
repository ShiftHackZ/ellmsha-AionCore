#![warn(clippy::disallowed_types)]

//! Extension registry: manifest parsing, hub installer, skill scanning, and lifecycle hooks.

mod asset_paths;
pub mod classifier;
pub mod clockify;
pub mod compatibility;
pub mod constants;
pub mod dependency;
pub mod error;
pub mod external_paths;
pub mod fs_adopt;
pub mod hub;
pub mod hub_routes;
pub mod lifecycle;
pub mod linear;
pub mod loader;
pub mod manifest;
pub mod notion;
pub mod permission;
pub mod plugin_installer;
pub mod registry;
mod registry_helpers;
pub mod resolvers;
pub mod routes;
pub mod skill_routes;
pub mod skill_service;
pub mod startup_materialize;
pub mod state;
pub mod template;
pub mod types;
pub mod watcher;

pub use classifier::{AssistantClassifier, AssistantRuleDispatcher, DefaultUserClassifier};
pub use clockify::{
    CLOCKIFY_ACCOUNT_EMAIL_ENV, CLOCKIFY_ACCOUNT_ID_ENV, CLOCKIFY_ACCOUNT_NAME_ENV, CLOCKIFY_API_KEY_ENV,
    CLOCKIFY_CONNECTION_ID_ENV, CLOCKIFY_LOCK_DIR_ENV, CLOCKIFY_PLUGIN_NAME, CLOCKIFY_REALM_ENV,
    ClockifyConnectionError, ClockifyConnectionManager, ClockifyConnectionProbe, ClockifyProbeResult,
    clockify_api_base, realm_name,
};
pub use constants::*;
pub use dependency::{DependencyIssue, DependencyValidationResult, topological_sort, validate_dependencies};
pub use error::ExtensionError;
pub use lifecycle::{HookKind, execute_hook, needs_install_hook, resolve_hook_path};
pub use linear::{
    LINEAR_MCP_URL, LINEAR_PLUGIN_NAME, LinearConnectionError, LinearConnectionManager, LinearConnectionProbe,
    LinearProbeFailure, LinearProbeResult, MCP_REMOTE_VERSION,
};
pub use loader::{
    ScanPath, filter_by_engine_compatibility, load_all, resolve_install_target_dir_for_data_dir,
    resolve_plugin_install_target_dir, resolve_plugin_scan_paths_for_data_dir, resolve_scan_paths,
    resolve_scan_paths_for_data_dir,
};
pub use manifest::{parse_manifest, validate_manifest};
pub use permission::{build_permission_summary, calculate_risk_level};
pub use plugin_installer::{InstalledPlugin, PluginInstallProvenance, PluginInstallSource, PluginInstaller};
pub use registry::{ExtensionRegistry, ExtensionSummary, LeasedLoadedExtension, PluginRuntimeSnapshot};
pub use resolvers::{resolve_all_contributions, resolve_extension_contributions, resolve_i18n_for_all};
pub use startup_materialize::materialize_if_needed;
pub use state::{ExtensionStateStore, load_states_from_file, resolve_state_file_path, save_states_to_file};
pub use template::{resolve_env_map, resolve_env_templates, resolve_file_reference};
pub use types::*;
pub use watcher::ExtensionWatcher;

pub use external_paths::ExternalPathsManager;
pub use hub::{HubIndexManager, HubInstaller};
pub use hub_routes::{HubRouterState, hub_routes};
pub use routes::{ExtensionRouterState, extension_routes};
pub use skill_routes::{SkillRouterState, skill_routes};
pub use skill_service::{
    BUILTIN_SKILLS_ENV_VAR, ExternalSkillSource, NamedPath, ResolvedAgentSkill, ScannedSkill, SkillListItem,
    SkillPaths, SkillSource, builtin_skills_corpus, builtin_skills_materialize_marker, delete_skill,
    delete_skill_with_repo, detect_and_count_external_skills, detect_common_skill_paths, export_skill_with_symlink,
    get_skill_paths, import_skill, import_skills_with_repo, link_workspace_skills, list_available_skills,
    list_available_skills_with_repo, list_available_skills_with_repo_for_user, materialize_skills_for_agent,
    materialize_skills_for_agent_with_repo, materialize_skills_for_agent_with_repo_for_user, read_builtin_rule,
    read_builtin_skill, read_skill_info, resolve_skill_paths, scan_for_skills, sync_builtin_skill_catalog_into_repo,
    sync_skill_catalog_into_repo, sync_skill_catalog_into_repo_for_user, sync_workspace_plugin_skills,
};
pub use skill_service::{
    delete_assistant_rule, delete_assistant_skill, read_assistant_rule, read_assistant_skill, write_assistant_rule,
    write_assistant_skill,
};

pub use notion::{
    NOTION_MCP_URL, NOTION_PLUGIN_NAME, NotionConnectionError, NotionConnectionManager, NotionConnectionProbe,
    NotionProbeFailure, NotionProbeResult,
};
