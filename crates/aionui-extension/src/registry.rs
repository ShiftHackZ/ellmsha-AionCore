use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aionui_api_types::{
    ClockifyConnectionResponse, ClockifyConnectionsResponse, ClockifyRealm, LinearConnectionResponse,
    LinearConnectionsResponse, NotionConnectionResponse, NotionConnectionsResponse, SessionMcpServer,
    SessionMcpTransport, WebSocketMessage,
};
use aionui_common::{TimestampMs, now_ms};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::clockify::{CLOCKIFY_PLUGIN_NAME, ClockifyConnectionError, ClockifyConnectionManager};
use crate::error::ExtensionError;
use crate::lifecycle::{HookKind, execute_hook, needs_install_hook, resolve_hook_path};
use crate::linear::{LINEAR_PLUGIN_NAME, LinearConnectionError, LinearConnectionManager};
use crate::loader::{ScanPath, load_plugin_directory, resolve_scan_paths};
use crate::notion::{NOTION_PLUGIN_NAME, NotionConnectionError, NotionConnectionManager};
use crate::plugin_installer::copy_plugin_tree;
use crate::registry_helpers::{
    build_state_map, load_and_validate, merge_persisted_states, run_deactivation_hooks, to_summary,
};
use crate::resolvers::{resolve_all_contributions, resolve_i18n_for_all};
use crate::state::ExtensionStateStore;
use crate::types::{
    ExtensionLifecyclePayload, ExtensionSource, ExtensionState, ExtensionSystemEvent, LoadedExtension,
    ResolvedAcpAdapter, ResolvedAgent, ResolvedAssistant, ResolvedChannelPlugin, ResolvedContributions,
    ResolvedModelProvider, ResolvedSettingsTab, ResolvedSkill, ResolvedTheme, WebuiContribution,
};

// Re-export ExtensionSummary from registry_helpers so that
// `registry::{ExtensionRegistry, ExtensionSummary}` continues to work.
pub use crate::registry_helpers::ExtensionSummary;

const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";

type MaterializedRevisions = (
    Vec<LoadedExtension>,
    HashMap<String, PathBuf>,
    HashMap<String, Arc<RevisionLease>>,
);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Central registry orchestrating extension loading, activation, contribution
/// resolution, and event broadcasting.
///
/// Thread-safe: can be shared across HTTP handlers, the file watcher, and
/// other async tasks via `Arc`.
#[derive(Clone)]
pub struct ExtensionRegistry {
    inner: Arc<RwLock<RegistryInner>>,
    state_store: ExtensionStateStore,
    broadcaster: Arc<dyn EventBroadcaster>,
    app_version: String,
    reload_lock: Arc<Mutex<()>>,
    revision_root: Arc<PathBuf>,
    revision_counter: Arc<AtomicU64>,
    linear_connections: Option<LinearConnectionManager>,
    clockify_connections: Option<ClockifyConnectionManager>,
    notion_connections: Option<NotionConnectionManager>,
}

struct RegistryInner {
    extensions: Vec<LoadedExtension>,
    source_directories: HashMap<String, PathBuf>,
    revision_leases: HashMap<String, Arc<RevisionLease>>,
    contributions: ResolvedContributions,
    scan_paths: Vec<ScanPath>,
    initialized: bool,
    generation: u64,
}

#[derive(Debug)]
struct RevisionLease {
    directory: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LeasedLoadedExtension {
    extension: LoadedExtension,
    _revision_lease: Option<Arc<RevisionLease>>,
}

impl Deref for LeasedLoadedExtension {
    type Target = LoadedExtension;

    fn deref(&self) -> &Self::Target {
        &self.extension
    }
}

impl Drop for RevisionLease {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(directory = %self.directory.display(), error = %error, "failed to prune plugin runtime revision");
        }
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Create a new registry.
    ///
    /// - `state_store`: persists enabled/disabled states across restarts.
    /// - `broadcaster`: pushes WebSocket events to connected clients.
    /// - `app_version`: current application version for engine compatibility.
    pub fn new(state_store: ExtensionStateStore, broadcaster: Arc<dyn EventBroadcaster>, app_version: String) -> Self {
        Self::new_with_linear_connections(state_store, broadcaster, app_version, None)
    }

    pub fn new_with_linear_connections(
        state_store: ExtensionStateStore,
        broadcaster: Arc<dyn EventBroadcaster>,
        app_version: String,
        linear_connections: Option<LinearConnectionManager>,
    ) -> Self {
        Self::new_with_plugin_connections(state_store, broadcaster, app_version, linear_connections, None)
    }

    pub fn new_with_plugin_connections(
        state_store: ExtensionStateStore,
        broadcaster: Arc<dyn EventBroadcaster>,
        app_version: String,
        linear_connections: Option<LinearConnectionManager>,
        clockify_connections: Option<ClockifyConnectionManager>,
    ) -> Self {
        Self::new_with_account_connections(
            state_store,
            broadcaster,
            app_version,
            linear_connections,
            clockify_connections,
            None,
        )
    }

    pub fn new_with_account_connections(
        state_store: ExtensionStateStore,
        broadcaster: Arc<dyn EventBroadcaster>,
        app_version: String,
        linear_connections: Option<LinearConnectionManager>,
        clockify_connections: Option<ClockifyConnectionManager>,
        notion_connections: Option<NotionConnectionManager>,
    ) -> Self {
        let revision_root = state_store
            .file_path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".plugin-runtime");
        Self {
            inner: Arc::new(RwLock::new(RegistryInner {
                extensions: Vec::new(),
                source_directories: HashMap::new(),
                revision_leases: HashMap::new(),
                contributions: ResolvedContributions::default(),
                scan_paths: Vec::new(),
                initialized: false,
                generation: 0,
            })),
            state_store,
            broadcaster,
            app_version,
            reload_lock: Arc::new(Mutex::new(())),
            revision_root: Arc::new(revision_root),
            revision_counter: Arc::new(AtomicU64::new(0)),
            linear_connections,
            clockify_connections,
            notion_connections,
        }
    }

    fn prepare_revision_root(&self) -> Result<(), ExtensionError> {
        if self.revision_root.exists() {
            std::fs::remove_dir_all(self.revision_root.as_ref())?;
        }
        std::fs::create_dir_all(self.revision_root.as_ref())?;
        Ok(())
    }

    fn materialize_revisions(&self, source_extensions: Vec<LoadedExtension>) -> MaterializedRevisions {
        let mut revisions = Vec::with_capacity(source_extensions.len());
        let mut source_directories = HashMap::with_capacity(source_extensions.len());
        let mut revision_leases = HashMap::with_capacity(source_extensions.len());

        for extension in source_extensions {
            let name = extension.manifest.name.clone();
            let source_directory = PathBuf::from(&extension.directory);
            match self.materialize_revision(&extension) {
                Ok((revision, lease)) => {
                    source_directories.insert(name.clone(), source_directory);
                    revision_leases.insert(name, lease);
                    revisions.push(revision);
                }
                Err(error) => warn!(
                    extension = %name,
                    directory = %source_directory.display(),
                    error = %error,
                    "plugin revision could not be validated; skipping candidate"
                ),
            }
        }

        (revisions, source_directories, revision_leases)
    }

    fn materialize_revision(
        &self,
        source: &LoadedExtension,
    ) -> Result<(LoadedExtension, Arc<RevisionLease>), ExtensionError> {
        let revision = self.revision_counter.fetch_add(1, Ordering::Relaxed);
        let process = std::process::id();
        let staging = self.revision_root.join(format!(".staging-{process}-{revision}"));
        let target = self.revision_root.join(format!("revision-{process}-{revision}"));

        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        if let Err(error) = copy_plugin_tree(Path::new(&source.directory), &staging) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&staging, &target) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error.into());
        }

        match load_plugin_directory(&target, source.source) {
            Ok(revision) => Ok((revision, Arc::new(RevisionLease { directory: target }))),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&target);
                Err(error)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Run the full initialization pipeline using auto-detected scan paths.
    ///
    /// Resolves scan paths from environment variables and platform defaults,
    /// then delegates to [`Self::initialize_with_scan_paths`].
    pub async fn initialize(&self) -> Result<(), ExtensionError> {
        let scan_paths = resolve_scan_paths();
        self.initialize_with_scan_paths(scan_paths).await
    }

    pub(crate) async fn install_validation_context(&self) -> (String, Vec<LoadedExtension>) {
        let extensions = self.inner.read().await.extensions.clone();
        (self.app_version.clone(), extensions)
    }

    /// Run the full initialization pipeline with explicit scan paths.
    ///
    /// Prefer this over [`Self::initialize`] when the caller already knows
    /// the extension directories (e.g., in tests or embedded deployments).
    ///
    /// Pipeline:
    /// 1. Load manifests from all directories
    /// 2. Filter by engine compatibility
    /// 3. Validate dependencies + topological sort
    /// 4. Merge persisted states (enabled/disabled)
    /// 5. Run lifecycle hooks (onInstall if needed, then onActivate)
    /// 6. Resolve all contributions
    /// 7. Persist updated states
    pub async fn initialize_with_scan_paths(&self, scan_paths: Vec<ScanPath>) -> Result<(), ExtensionError> {
        let _reload_guard = self.reload_lock.lock().await;
        info!("initializing extension registry");
        debug!(count = scan_paths.len(), "resolved scan paths");

        // 1-3. Load, filter, validate (all sync/blocking).
        let (source_extensions, dep_result) = load_and_validate(&scan_paths, &self.app_version);
        self.prepare_revision_root()?;
        let (extensions, source_directories, revision_leases) = self.materialize_revisions(source_extensions);

        // 4. Merge persisted states.
        let persisted = self.state_store.load().await?;
        let extensions = merge_persisted_states(extensions, &persisted);

        // 5. Run lifecycle hooks.
        let extensions = self.run_activation_hooks(extensions, &persisted).await;

        // 6. Resolve contributions.
        let contributions = resolve_all_installed_contributions(&extensions);

        // 7. Persist updated states.
        let states = build_state_map(&extensions);
        self.state_store.set_all(states).await;

        // Commit to inner state.
        {
            let mut guard = self.inner.write().await;
            guard.extensions = extensions;
            guard.source_directories = source_directories;
            guard.revision_leases = revision_leases;
            guard.contributions = contributions;
            guard.scan_paths = scan_paths;
            guard.initialized = true;
            guard.generation = guard.generation.saturating_add(1);
        }

        if !dep_result.issues.is_empty() {
            warn!(issues = dep_result.issues.len(), "dependency validation found issues");
        }

        info!("extension registry initialized");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hot reload
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Hot-reload the registry: deactivate all -> clear -> reload -> re-resolve.
    ///
    /// Emits `REGISTRY_RELOADED` event when complete.
    pub async fn hot_reload(&self) {
        let _reload_guard = self.reload_lock.lock().await;
        info!("hot-reloading extension registry");

        // 1. Snapshot current extensions and scan paths for deactivation.
        let (current_exts, current_sources, current_leases, scan_paths) = {
            let guard = self.inner.read().await;
            (
                guard.extensions.clone(),
                guard.source_directories.clone(),
                guard.revision_leases.clone(),
                guard.scan_paths.clone(),
            )
        };

        // 2. Run onDeactivate hooks for each currently active extension.
        run_deactivation_hooks(&current_exts).await;

        // 3. Reload pipeline (same as initialize but reuses existing scan paths).
        let (source_extensions, _dep_result) = load_and_validate(&scan_paths, &self.app_version);
        let (mut extensions, mut source_directories, mut revision_leases) =
            self.materialize_revisions(source_extensions);
        let replacements: HashSet<String> = extensions
            .iter()
            .map(|extension| extension.manifest.name.clone())
            .collect();

        // Filesystem events can arrive while an editor is between truncate and
        // rewrite. Retain a malformed update while its source directory still
        // exists, but treat removal of the source directory as an uninstall.
        // Runtime revisions are immutable, so retained entries keep every
        // skill, MCP script, and lifecycle resource they were resolved from.
        for current in current_exts {
            if !replacements.contains(&current.manifest.name) {
                let source_directory = current_sources
                    .get(&current.manifest.name)
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from(&current.directory));
                let source_removed = match std::fs::metadata(&source_directory) {
                    Ok(metadata) => !metadata.is_dir(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => {
                        warn!(
                            extension = %current.manifest.name,
                            directory = %source_directory.display(),
                            error = %error,
                            "plugin source could not be inspected; retaining last working version"
                        );
                        false
                    }
                };
                if source_removed {
                    info!(
                        extension = %current.manifest.name,
                        directory = %source_directory.display(),
                        "plugin source removed; unloading last working version"
                    );
                    continue;
                }
                warn!(
                    extension = %current.manifest.name,
                    directory = %source_directory.display(),
                    "plugin update is invalid; retaining last working version"
                );
                source_directories.insert(current.manifest.name.clone(), source_directory);
                if let Some(lease) = current_leases.get(&current.manifest.name) {
                    revision_leases.insert(current.manifest.name.clone(), Arc::clone(lease));
                }
                extensions.push(current);
            }
        }

        // Use in-memory state (not file) to preserve pending writes that
        // haven't been flushed yet by the debounce timer.
        let persisted = self.state_store.get_all().await;

        let extensions = merge_persisted_states(extensions, &persisted);
        let extensions = self.run_activation_hooks(extensions, &persisted).await;
        let contributions = resolve_all_installed_contributions(&extensions);

        let states = build_state_map(&extensions);
        self.state_store.set_all(states).await;

        // 4. Commit new state.
        {
            let mut guard = self.inner.write().await;
            guard.extensions = extensions;
            guard.source_directories = source_directories;
            guard.revision_leases = revision_leases;
            guard.contributions = contributions;
            guard.generation = guard.generation.saturating_add(1);
            // scan_paths stay the same
        }

        // 5. Broadcast REGISTRY_RELOADED event.
        self.broadcast_lifecycle_event("registry", ExtensionSystemEvent::RegistryReloaded, None);

        info!("extension registry hot-reloaded");
    }
}

// ---------------------------------------------------------------------------
// Enable / Disable
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Enable an extension by name.
    ///
    /// Compatibility wrapper for the local system user.
    pub async fn enable_extension(&self, name: &str) -> Result<(), ExtensionError> {
        self.enable_extension_for_user(SYSTEM_DEFAULT_USER_ID, name).await
    }

    /// Enable an installed extension for one application user.
    pub async fn enable_extension_for_user(&self, user_id: &str, name: &str) -> Result<(), ExtensionError> {
        let _runtime_guard = self.reload_lock.lock().await;
        self.require_installed(name).await?;
        if self.extension_enabled_for_user(user_id, name).await {
            debug!(user_id, name, "extension already enabled for user");
            return Ok(());
        }

        self.state_store.set_user_enabled(user_id, name, true).await?;
        self.bump_generation().await;
        self.broadcast_state_changed(user_id, name, true);

        info!(user_id, name, "extension enabled for user");
        Ok(())
    }

    /// Disable an extension by name.
    ///
    /// Compatibility wrapper for the local system user.
    pub async fn disable_extension(&self, name: &str, reason: Option<&str>) -> Result<(), ExtensionError> {
        self.disable_extension_for_user(SYSTEM_DEFAULT_USER_ID, name, reason)
            .await
    }

    /// Disable an installed extension for one application user.
    pub async fn disable_extension_for_user(
        &self,
        user_id: &str,
        name: &str,
        reason: Option<&str>,
    ) -> Result<(), ExtensionError> {
        let _runtime_guard = self.reload_lock.lock().await;
        self.require_installed(name).await?;
        if !self.extension_enabled_for_user(user_id, name).await {
            debug!(user_id, name, "extension already disabled for user");
            return Ok(());
        }

        self.state_store.set_user_enabled(user_id, name, false).await?;
        self.bump_generation().await;
        self.broadcast_state_changed(user_id, name, false);

        info!(
            user_id,
            name,
            reason_supplied = reason.is_some(),
            "extension disabled for user"
        );
        Ok(())
    }

    async fn require_installed(&self, name: &str) -> Result<(), ExtensionError> {
        let guard = self.inner.read().await;
        guard
            .extensions
            .iter()
            .any(|extension| extension.manifest.name == name)
            .then_some(())
            .ok_or_else(|| ExtensionError::NotFound(name.to_owned()))
    }

    async fn extension_enabled_for_user(&self, user_id: &str, name: &str) -> bool {
        if let Some(enabled) = self.state_store.get_user_enabled(user_id, name).await {
            return enabled;
        }
        if user_id != SYSTEM_DEFAULT_USER_ID {
            return true;
        }
        let guard = self.inner.read().await;
        guard
            .extensions
            .iter()
            .find(|extension| extension.manifest.name == name)
            .is_none_or(|extension| extension.state.enabled)
    }

    /// Enable or disable an extension only for one project. The project value
    /// overrides the user's global value and is inherited by every conversation
    /// and subagent bound to that project.
    pub async fn set_extension_enabled_for_project(
        &self,
        user_id: &str,
        project_id: &str,
        name: &str,
        enabled: bool,
    ) -> Result<(), ExtensionError> {
        let _runtime_guard = self.reload_lock.lock().await;
        self.require_installed(name).await?;
        if project_id.trim().is_empty() {
            return Err(ExtensionError::InvalidRequest("project_id must not be empty".into()));
        }
        self.state_store
            .set_project_enabled(user_id, project_id, name, enabled)
            .await?;
        self.bump_generation().await;
        self.broadcast_state_changed(user_id, name, enabled);
        Ok(())
    }

    async fn bump_generation(&self) {
        let mut guard = self.inner.write().await;
        guard.generation = guard.generation.saturating_add(1);
    }
}

// ---------------------------------------------------------------------------
// Query methods
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Return summaries of all loaded extensions.
    pub async fn get_loaded_extensions(&self) -> Vec<ExtensionSummary> {
        self.get_loaded_extensions_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    /// Return summaries with enabled state overlaid for one user.
    pub async fn get_loaded_extensions_for_user(&self, user_id: &str) -> Vec<ExtensionSummary> {
        self.extensions_for_user(user_id).await.iter().map(to_summary).collect()
    }

    pub(crate) fn event_broadcaster(&self) -> Arc<dyn EventBroadcaster> {
        self.broadcaster.clone()
    }

    /// Look up a single loaded extension by name.
    pub async fn get_extension_by_name(&self, name: &str) -> Option<LeasedLoadedExtension> {
        let _runtime_guard = self.reload_lock.lock().await;
        let guard = self.inner.read().await;
        let extension = guard.extensions.iter().find(|e| e.manifest.name == name)?.clone();
        let lease = guard.revision_leases.get(name).cloned();
        Some(LeasedLoadedExtension {
            extension,
            _revision_lease: lease,
        })
    }

    /// Look up an installed extension with one user's enabled state overlaid.
    pub async fn get_extension_by_name_for_user(&self, user_id: &str, name: &str) -> Option<LeasedLoadedExtension> {
        let _runtime_guard = self.reload_lock.lock().await;
        let (mut extension, lease) = {
            let guard = self.inner.read().await;
            let extension = guard.extensions.iter().find(|e| e.manifest.name == name)?.clone();
            let lease = guard.revision_leases.get(name).cloned();
            (extension, lease)
        };
        extension.state.enabled = self
            .state_store
            .get_user_enabled(user_id, name)
            .await
            .unwrap_or(user_id != SYSTEM_DEFAULT_USER_ID || extension.state.enabled);
        Some(LeasedLoadedExtension {
            extension,
            _revision_lease: lease,
        })
    }

    /// Snapshot of all resolved contributions.
    pub async fn get_contributions(&self) -> ResolvedContributions {
        self.get_contributions_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_contributions_for_user(&self, user_id: &str) -> ResolvedContributions {
        let enabled: HashSet<String> = self
            .extensions_for_user(user_id)
            .await
            .into_iter()
            .filter(|extension| extension.state.enabled)
            .map(|extension| extension.manifest.name)
            .collect();
        let mut contributions = {
            let guard = self.inner.read().await;
            guard.contributions.clone()
        };
        retain_contributions(&mut contributions, &enabled);
        contributions
    }

    pub async fn get_contributions_for_binding(
        &self,
        user_id: &str,
        project_id: Option<&str>,
    ) -> ResolvedContributions {
        let enabled: HashSet<String> = self
            .extensions_for_binding(user_id, project_id)
            .await
            .into_iter()
            .filter(|extension| extension.state.enabled)
            .map(|extension| extension.manifest.name)
            .collect();
        let mut contributions = {
            let guard = self.inner.read().await;
            guard.contributions.clone()
        };
        retain_contributions(&mut contributions, &enabled);
        contributions
    }

    /// Snapshot the skills and MCP servers that a new agent session should use.
    /// Callers keep this immutable snapshot for the duration of a turn.
    pub async fn runtime_snapshot(&self, user_id: &str, project_id: Option<&str>) -> PluginRuntimeSnapshot {
        let _runtime_guard = self.reload_lock.lock().await;
        let linear_extension = self
            .extensions_for_user(user_id)
            .await
            .into_iter()
            .find(|extension| extension.state.enabled && extension.manifest.name == LINEAR_PLUGIN_NAME);
        let clockify_enabled = self
            .extensions_for_user(user_id)
            .await
            .into_iter()
            .any(|extension| extension.state.enabled && extension.manifest.name == CLOCKIFY_PLUGIN_NAME);
        let notion_enabled = self
            .extensions_for_user(user_id)
            .await
            .into_iter()
            .any(|extension| extension.state.enabled && extension.manifest.name == NOTION_PLUGIN_NAME);
        let contributions = self.get_contributions_for_binding(user_id, project_id).await;
        let (generation, revision_leases) = {
            let guard = self.inner.read().await;
            (
                guard.generation,
                guard.revision_leases.values().cloned().collect::<Vec<_>>(),
            )
        };
        let (linear_revision, linear_servers) = match &self.linear_connections {
            Some(manager) => manager.runtime_servers_snapshot(user_id).await,
            None => (0, Vec::new()),
        };
        let (clockify_revision, clockify_servers) = match &self.clockify_connections {
            Some(manager) => manager.runtime_servers_snapshot(user_id).await,
            None => (0, Vec::new()),
        };
        let (notion_revision, notion_servers) = match &self.notion_connections {
            Some(manager) => manager.runtime_servers_snapshot(user_id).await,
            None => (0, Vec::new()),
        };
        let skills = contributions
            .skills
            .into_iter()
            .filter_map(resolved_skill_to_agent_skill)
            .collect();
        let mut mcp_servers: Vec<_> = contributions
            .mcp_servers
            .into_iter()
            .filter_map(resolved_mcp_to_session)
            .collect();
        if let Some(extension) = linear_extension {
            mcp_servers.extend(
                linear_servers
                    .into_iter()
                    .map(|server| crate::linear::with_image_normalization(server, Path::new(&extension.directory))),
            );
        }
        if clockify_enabled {
            mcp_servers.extend(clockify_servers);
        }
        if notion_enabled {
            mcp_servers.extend(notion_servers);
        }
        PluginRuntimeSnapshot {
            generation,
            linear_revision: combined_connection_revision(linear_revision, clockify_revision)
                ^ notion_revision.rotate_left(31),
            skills,
            mcp_servers,
            _revision_leases: revision_leases,
        }
    }

    pub async fn get_themes(&self) -> Vec<ResolvedTheme> {
        self.get_themes_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_themes_for_user(&self, user_id: &str) -> Vec<ResolvedTheme> {
        self.get_contributions_for_user(user_id).await.themes
    }

    pub async fn get_assistants(&self) -> Vec<ResolvedAssistant> {
        self.get_assistants_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_assistants_for_user(&self, user_id: &str) -> Vec<ResolvedAssistant> {
        self.get_contributions_for_user(user_id).await.assistants
    }

    /// Return `true` if any extension contributes an assistant with this id.
    pub async fn has_assistant(&self, id: &str) -> bool {
        self.has_assistant_for_user(SYSTEM_DEFAULT_USER_ID, id).await
    }

    pub async fn has_assistant_for_user(&self, user_id: &str, id: &str) -> bool {
        self.get_assistants_for_user(user_id)
            .await
            .iter()
            .any(|assistant| assistant.id == id)
    }

    /// Lookup a single extension-contributed assistant by id.
    pub async fn get_assistant_by_id(&self, id: &str) -> Option<ResolvedAssistant> {
        self.get_assistant_by_id_for_user(SYSTEM_DEFAULT_USER_ID, id).await
    }

    pub async fn get_assistant_by_id_for_user(&self, user_id: &str, id: &str) -> Option<ResolvedAssistant> {
        self.get_assistants_for_user(user_id)
            .await
            .into_iter()
            .find(|assistant| assistant.id == id)
    }

    pub async fn get_acp_adapters(&self) -> Vec<ResolvedAcpAdapter> {
        self.get_acp_adapters_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_acp_adapters_for_user(&self, user_id: &str) -> Vec<ResolvedAcpAdapter> {
        self.get_contributions_for_user(user_id).await.acp_adapters
    }

    pub async fn get_agents(&self) -> Vec<ResolvedAgent> {
        self.get_agents_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_agents_for_user(&self, user_id: &str) -> Vec<ResolvedAgent> {
        self.get_contributions_for_user(user_id).await.agents
    }

    pub async fn get_mcp_servers(&self) -> Vec<crate::types::ResolvedMcpServer> {
        self.get_mcp_servers_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_mcp_servers_for_user(&self, user_id: &str) -> Vec<crate::types::ResolvedMcpServer> {
        self.get_contributions_for_user(user_id).await.mcp_servers
    }

    pub async fn get_linear_technical_servers_for_user(&self, user_id: &str) -> Vec<SessionMcpServer> {
        let enabled = self
            .extensions_for_user(user_id)
            .await
            .into_iter()
            .any(|extension| extension.state.enabled && extension.manifest.name == LINEAR_PLUGIN_NAME);
        if !enabled {
            return Vec::new();
        }
        match &self.linear_connections {
            Some(manager) => manager.technical_servers(user_id).await,
            None => Vec::new(),
        }
    }

    pub async fn get_notion_technical_servers_for_user(&self, user_id: &str) -> Vec<SessionMcpServer> {
        let enabled = self
            .extensions_for_user(user_id)
            .await
            .into_iter()
            .any(|extension| extension.state.enabled && extension.manifest.name == NOTION_PLUGIN_NAME);
        if !enabled {
            return Vec::new();
        }
        match &self.notion_connections {
            Some(manager) => manager.technical_servers(user_id).await,
            None => Vec::new(),
        }
    }

    pub async fn list_linear_connections(
        &self,
        user_id: &str,
    ) -> Result<LinearConnectionsResponse, LinearConnectionError> {
        self.linear_manager()?.list(user_id).await
    }

    pub async fn connect_new_linear_connection(
        &self,
        user_id: &str,
    ) -> Result<LinearConnectionResponse, LinearConnectionError> {
        self.linear_manager()?.connect_new(user_id).await
    }

    pub async fn migrate_legacy_linear_connection(
        &self,
        user_id: &str,
    ) -> Result<LinearConnectionsResponse, LinearConnectionError> {
        self.linear_manager()?.migrate_legacy(user_id).await
    }

    pub async fn connect_linear_connection(
        &self,
        user_id: &str,
        connection_id: &str,
        reconnect: bool,
    ) -> Result<LinearConnectionResponse, LinearConnectionError> {
        self.linear_manager()?.connect(user_id, connection_id, reconnect).await
    }

    pub async fn disconnect_linear_connection(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<LinearConnectionResponse, LinearConnectionError> {
        self.linear_manager()?.disconnect(user_id, connection_id).await
    }

    fn linear_manager(&self) -> Result<&LinearConnectionManager, LinearConnectionError> {
        self.linear_connections.as_ref().ok_or(LinearConnectionError::NotFound)
    }

    pub async fn list_notion_connections(
        &self,
        user_id: &str,
    ) -> Result<NotionConnectionsResponse, NotionConnectionError> {
        self.notion_manager()?.list(user_id).await
    }

    pub async fn connect_new_notion_connection(
        &self,
        user_id: &str,
    ) -> Result<NotionConnectionResponse, NotionConnectionError> {
        self.notion_manager()?.connect_new(user_id).await
    }

    pub async fn connect_notion_connection(
        &self,
        user_id: &str,
        connection_id: &str,
        reconnect: bool,
    ) -> Result<NotionConnectionResponse, NotionConnectionError> {
        self.notion_manager()?.connect(user_id, connection_id, reconnect).await
    }

    pub async fn disconnect_notion_connection(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<NotionConnectionResponse, NotionConnectionError> {
        self.notion_manager()?.disconnect(user_id, connection_id).await
    }

    fn notion_manager(&self) -> Result<&NotionConnectionManager, NotionConnectionError> {
        self.notion_connections.as_ref().ok_or(NotionConnectionError::NotFound)
    }

    pub async fn list_clockify_connections(
        &self,
        user_id: &str,
    ) -> Result<ClockifyConnectionsResponse, ClockifyConnectionError> {
        self.clockify_manager()?.list(user_id).await
    }

    pub async fn connect_new_clockify_connection(
        &self,
        user_id: &str,
        realm: ClockifyRealm,
        api_key: String,
    ) -> Result<ClockifyConnectionResponse, ClockifyConnectionError> {
        self.clockify_manager()?.connect_new(user_id, realm, api_key).await
    }

    pub async fn reconnect_clockify_connection(
        &self,
        user_id: &str,
        connection_id: &str,
        realm: ClockifyRealm,
        api_key: String,
    ) -> Result<ClockifyConnectionResponse, ClockifyConnectionError> {
        self.clockify_manager()?
            .reconnect(user_id, connection_id, realm, api_key)
            .await
    }

    pub async fn disconnect_clockify_connection(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<ClockifyConnectionResponse, ClockifyConnectionError> {
        self.clockify_manager()?.disconnect(user_id, connection_id).await
    }

    fn clockify_manager(&self) -> Result<&ClockifyConnectionManager, ClockifyConnectionError> {
        self.clockify_connections
            .as_ref()
            .ok_or(ClockifyConnectionError::NotFound)
    }

    pub async fn get_skills(&self) -> Vec<ResolvedSkill> {
        self.get_skills_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_skills_for_user(&self, user_id: &str) -> Vec<ResolvedSkill> {
        self.get_contributions_for_user(user_id).await.skills
    }

    pub async fn get_settings_tabs(&self) -> Vec<ResolvedSettingsTab> {
        self.get_settings_tabs_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_settings_tabs_for_user(&self, user_id: &str) -> Vec<ResolvedSettingsTab> {
        self.get_contributions_for_user(user_id).await.settings_tabs
    }

    pub async fn get_webui_contributions(&self) -> Vec<WebuiContribution> {
        self.get_webui_contributions_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_webui_contributions_for_user(&self, user_id: &str) -> Vec<WebuiContribution> {
        self.get_contributions_for_user(user_id).await.webui
    }

    pub async fn get_channel_plugins(&self) -> Vec<ResolvedChannelPlugin> {
        self.get_channel_plugins_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_channel_plugins_for_user(&self, user_id: &str) -> Vec<ResolvedChannelPlugin> {
        self.get_contributions_for_user(user_id).await.channel_plugins
    }

    pub async fn get_model_providers(&self) -> Vec<ResolvedModelProvider> {
        self.get_model_providers_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_model_providers_for_user(&self, user_id: &str) -> Vec<ResolvedModelProvider> {
        self.get_contributions_for_user(user_id).await.model_providers
    }

    /// Resolve i18n data for a given locale across all enabled extensions.
    pub async fn get_i18n_for_locale(&self, locale: &str) -> HashMap<String, HashMap<String, String>> {
        self.get_i18n_for_locale_for_user(SYSTEM_DEFAULT_USER_ID, locale).await
    }

    pub async fn get_i18n_for_locale_for_user(
        &self,
        user_id: &str,
        locale: &str,
    ) -> HashMap<String, HashMap<String, String>> {
        resolve_i18n_for_all(&self.extensions_for_user(user_id).await, locale)
    }

    /// Whether the registry has been initialized.
    pub async fn is_initialized(&self) -> bool {
        let guard = self.inner.read().await;
        guard.initialized
    }

    async fn extensions_for_user(&self, user_id: &str) -> Vec<LoadedExtension> {
        let mut extensions = {
            let guard = self.inner.read().await;
            guard.extensions.clone()
        };
        let overrides = self.state_store.get_user_enabled_all(user_id).await;
        for extension in &mut extensions {
            extension.state.enabled = overrides
                .get(&extension.manifest.name)
                .copied()
                .unwrap_or(user_id != SYSTEM_DEFAULT_USER_ID || extension.state.enabled);
        }
        extensions
    }

    async fn extensions_for_binding(&self, user_id: &str, project_id: Option<&str>) -> Vec<LoadedExtension> {
        let mut extensions = self.extensions_for_user(user_id).await;
        if let Some(project_id) = project_id {
            let overrides = self.state_store.get_project_enabled_all(user_id, project_id).await;
            for extension in &mut extensions {
                if let Some(enabled) = overrides.get(&extension.manifest.name) {
                    extension.state.enabled = *enabled;
                }
            }
        }
        extensions
    }
}

#[derive(Debug, Clone)]
pub struct PluginRuntimeSnapshot {
    pub generation: u64,
    pub linear_revision: u64,
    pub skills: Vec<crate::skill_service::ResolvedAgentSkill>,
    pub mcp_servers: Vec<SessionMcpServer>,
    _revision_leases: Vec<Arc<RevisionLease>>,
}

fn resolved_skill_to_agent_skill(skill: ResolvedSkill) -> Option<crate::skill_service::ResolvedAgentSkill> {
    let path = Path::new(skill.path.as_deref()?);
    let source_path = if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
        path.parent()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    Some(crate::skill_service::ResolvedAgentSkill {
        name: skill.name,
        source_path,
    })
}

fn combined_connection_revision(linear: u64, clockify: u64) -> u64 {
    linear.rotate_left(17) ^ clockify
}

fn resolved_mcp_to_session(server: crate::types::ResolvedMcpServer) -> Option<SessionMcpServer> {
    if server.config.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) {
        return None;
    }
    let config = server.config.get("transport").unwrap_or(&server.config);
    let transport_type = config
        .get("type")
        .or_else(|| config.get("transportType"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| {
            if config.get("command").is_some() {
                "stdio"
            } else {
                "streamable_http"
            }
        });
    let string_map = |value: Option<&serde_json::Value>| {
        value
            .and_then(serde_json::Value::as_object)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().and_then(|value| {
                            crate::template::resolve_env_templates(value, false)
                                .ok()
                                .map(|value| (key.clone(), value))
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let transport = match transport_type {
        "stdio" => SessionMcpTransport::Stdio {
            command: config.get("command")?.as_str()?.to_owned(),
            args: config
                .get("args")
                .and_then(serde_json::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            env: string_map(config.get("env")),
        },
        "http" => SessionMcpTransport::Http {
            url: config.get("url")?.as_str()?.to_owned(),
            headers: string_map(config.get("headers")),
        },
        "sse" => SessionMcpTransport::Sse {
            url: config.get("url")?.as_str()?.to_owned(),
            headers: string_map(config.get("headers")),
        },
        "streamable_http" => SessionMcpTransport::StreamableHttp {
            url: config.get("url")?.as_str()?.to_owned(),
            headers: string_map(config.get("headers")),
        },
        _ => return None,
    };
    let runtime_id = format!("plugin:{}:{}", server.extension_name, server.id);
    Some(SessionMcpServer {
        id: runtime_id.clone(),
        name: runtime_id,
        transport,
        approval_identity: None,
        auto_approve_in_unattended: server.auto_approve_in_unattended,
    })
}

fn resolve_all_installed_contributions(extensions: &[LoadedExtension]) -> ResolvedContributions {
    let mut installed = extensions.to_vec();
    for extension in &mut installed {
        extension.state.enabled = true;
    }
    resolve_all_contributions(&installed)
}

fn retain_contributions(contributions: &mut ResolvedContributions, enabled: &HashSet<String>) {
    contributions
        .acp_adapters
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .mcp_servers
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .assistants
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .agents
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .skills
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .themes
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .channel_plugins
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .webui
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .settings_tabs
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .model_providers
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .i18n
        .retain(|extension_name, _| enabled.contains(extension_name));
}

// ---------------------------------------------------------------------------
// Event broadcasting helpers
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    fn broadcast_state_changed(&self, user_id: &str, name: &str, enabled: bool) {
        let event = WebSocketMessage::new(
            "extensions.state-changed",
            json!({ "user_id": user_id, "name": name, "enabled": enabled }),
        );
        self.broadcaster.broadcast(event);
    }

    fn broadcast_lifecycle_event(
        &self,
        extension_name: &str,
        event: ExtensionSystemEvent,
        data: Option<serde_json::Value>,
    ) {
        let payload = ExtensionLifecyclePayload {
            extension_name: extension_name.to_owned(),
            event,
            timestamp: now_ms(),
            data,
        };
        let msg = WebSocketMessage::new(
            "extensions.lifecycle",
            serde_json::to_value(&payload).unwrap_or_default(),
        );
        self.broadcaster.broadcast(msg);
    }
}

// ---------------------------------------------------------------------------
// Activation hooks
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Run lifecycle hooks for each extension in order:
    /// - `onInstall` if first time or version changed
    /// - `onActivate` for each enabled extension
    ///
    /// Hook failures are logged but do not prevent other extensions from
    /// activating.
    async fn run_activation_hooks(
        &self,
        mut extensions: Vec<LoadedExtension>,
        persisted: &HashMap<String, ExtensionState>,
    ) -> Vec<LoadedExtension> {
        let now: TimestampMs = now_ms();

        for ext in &mut extensions {
            if !ext.state.enabled {
                continue;
            }

            let ext_name = ext.manifest.name.clone();
            let ext_dir = Path::new(&ext.directory);

            // Check onInstall + onActivate hooks.
            if ext.source == ExtensionSource::Builtin
                && let Some(hooks) = &ext.manifest.lifecycle
            {
                let persisted_version = persisted.get(&ext_name).map(|s| s.version.as_str());

                if needs_install_hook(&ext.manifest.version, persisted_version)
                    && let Some(hook_path) = resolve_hook_path(hooks, HookKind::OnInstall)
                    && let Err(e) = execute_hook(ext_dir, hook_path, HookKind::OnInstall, &ext_name).await
                {
                    warn!(
                        extension = %ext_name,
                        error = %e,
                        "onInstall hook failed, continuing"
                    );
                }

                // Run onActivate
                if let Some(hook_path) = resolve_hook_path(hooks, HookKind::OnActivate)
                    && let Err(e) = execute_hook(ext_dir, hook_path, HookKind::OnActivate, &ext_name).await
                {
                    warn!(
                        extension = %ext_name,
                        error = %e,
                        "onActivate hook failed, continuing"
                    );
                }
            }

            // Update activation timestamp and install time.
            ext.state.last_activated_at = Some(now);
            if ext.state.installed_at.is_none() {
                ext.state.installed_at = Some(now);
            }

            self.broadcast_lifecycle_event(&ext_name, ExtensionSystemEvent::ExtensionActivated, None);
        }

        extensions
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExtensionManifest, ExtensionSource, ExtensionState};
    use aionui_realtime::BroadcastEventBus;

    fn make_test_ext(name: &str, enabled: bool) -> LoadedExtension {
        LoadedExtension {
            manifest: ExtensionManifest {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                display_name: Some(format!("{name} Display")),
                description: Some(format!("{name} description")),
                author: None,
                license: None,
                homepage: None,
                icon: None,
                engine: None,
                api_version: None,
                dependencies: HashMap::new(),
                entry_point: None,
                permissions: None,
                contributes: None,
                lifecycle: None,
                i18n: None,
                compatibility: None,
            },
            directory: format!("/tmp/ext/{name}"),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                enabled,
                installed_at: Some(1000),
                last_activated_at: None,
            },
        }
    }

    fn make_registry() -> (ExtensionRegistry, ExtensionStateStore, Arc<BroadcastEventBus>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store.clone(), bus.clone(), "1.0.0".to_owned());
        (registry, store, bus)
    }

    // -- enable_extension / disable_extension -----------------------------------

    #[tokio::test]
    async fn enable_nonexistent_returns_not_found() {
        let (registry, _, _) = make_registry();
        let result = registry.enable_extension("no-such-ext").await;
        assert!(matches!(result, Err(ExtensionError::NotFound(_))));
    }

    #[tokio::test]
    async fn disable_nonexistent_returns_not_found() {
        let (registry, _, _) = make_registry();
        let result = registry.disable_extension("no-such-ext", None).await;
        assert!(matches!(result, Err(ExtensionError::NotFound(_))));
    }

    #[tokio::test]
    async fn enable_disable_roundtrip() {
        let (registry, _, bus) = make_registry();

        // Seed the registry with a disabled extension.
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("test-ext", false)];
            guard.initialized = true;
        }

        let mut rx = bus.subscribe();

        // Enable
        registry.enable_extension("test-ext").await.unwrap();
        assert!(registry.get_loaded_extensions().await[0].enabled);

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.name, "extensions.state-changed");
        assert_eq!(msg.data["user_id"], SYSTEM_DEFAULT_USER_ID);
        assert_eq!(msg.data["enabled"], true);

        // Disable
        registry
            .disable_extension("test-ext", Some("test reason"))
            .await
            .unwrap();
        assert!(!registry.get_loaded_extensions().await[0].enabled);

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.name, "extensions.state-changed");
        assert_eq!(msg.data["enabled"], false);
    }

    #[tokio::test]
    async fn enable_already_enabled_is_noop() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext", true)];
        }
        // Should succeed without error.
        registry.enable_extension("ext").await.unwrap();
    }

    #[tokio::test]
    async fn disable_already_disabled_is_noop() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext", false)];
        }
        registry.disable_extension("ext", None).await.unwrap();
    }

    // -- query methods --------------------------------------------------------

    #[tokio::test]
    async fn get_loaded_extensions_returns_summaries() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext-a", true), make_test_ext("ext-b", false)];
        }

        let summaries = registry.get_loaded_extensions().await;
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "ext-a");
        assert!(summaries[0].enabled);
        assert_eq!(summaries[1].name, "ext-b");
        assert!(!summaries[1].enabled);
    }

    #[tokio::test]
    async fn get_extension_by_name_found_and_not_found() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("my-ext", true)];
        }

        assert!(registry.get_extension_by_name("my-ext").await.is_some());
        assert!(registry.get_extension_by_name("nope").await.is_none());
    }

    #[tokio::test]
    async fn is_initialized_before_and_after() {
        let (registry, _, _) = make_registry();
        assert!(!registry.is_initialized().await);

        {
            let mut guard = registry.inner.write().await;
            guard.initialized = true;
        }
        assert!(registry.is_initialized().await);
    }
}
