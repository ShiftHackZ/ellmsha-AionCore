use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tracing::{debug, warn};

use crate::constants::{EXTENSION_API_VERSION, EXTENSION_MANIFEST_FILE, EXTENSIONS_DIR_NAME};
use crate::manifest::parse_manifest_in_dir;
use crate::types::{ExtensionSource, ExtensionState, LoadedExtension};

// ---------------------------------------------------------------------------
// Scan path resolution
// ---------------------------------------------------------------------------

/// A scan path paired with its source classification.
#[derive(Debug, Clone)]
pub struct ScanPath {
    pub path: PathBuf,
    pub source: ExtensionSource,
}

/// Resolve the default list of directories to scan for extensions.
///
/// Priority (highest first):
/// 1. `$AIONUI_EXTENSIONS_PATH`
/// 2. `~/.aionui/extensions/` — legacy user data directory
/// 3. Platform AppData directory
///
/// In E2E test mode (`AIONUI_E2E_TEST=1`), only the environment variable
/// paths are returned to ensure test isolation.
pub fn resolve_scan_paths() -> Vec<ScanPath> {
    let env_path = std::env::var("AIONUI_EXTENSIONS_PATH").ok();
    let e2e_mode = is_e2e_test_mode();
    resolve_scan_paths_inner(env_path.as_deref(), e2e_mode, None)
}

/// Resolve scan paths using the historical Electron desktop rules for the
/// provided `data_dir`.
///
/// Priority (highest first):
/// 1. `$AIONUI_EXTENSIONS_PATH`
/// 2. `<data_dir>/extensions`
/// 3. Legacy appData sibling directory derived from `<data_dir>`
///
/// In E2E test mode (`AIONUI_E2E_TEST=1`), only the environment variable
/// paths are returned to ensure test isolation.
pub fn resolve_scan_paths_for_data_dir(data_dir: &Path) -> Vec<ScanPath> {
    let env_path = std::env::var("AIONUI_EXTENSIONS_PATH").ok();
    let e2e_mode = is_e2e_test_mode();
    resolve_scan_paths_inner(env_path.as_deref(), e2e_mode, Some(data_dir))
}

/// Resolve the plugin directories used by eLLMsha.
///
/// Built-in plugins live in the v3 repository's `plugins/` directory while
/// user-installed plugins live in the persistent `<userData>/plugins/`
/// directory. Legacy extension locations remain at lower priority so existing
/// installations continue to load.
pub fn resolve_plugin_scan_paths_for_data_dir(data_dir: &Path) -> Vec<ScanPath> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |path: PathBuf, source: ExtensionSource| {
        if seen.insert(path.clone()) {
            paths.push(ScanPath { path, source });
        }
    };

    if let Some(env_paths) = std::env::var_os("AIONUI_EXTENSIONS_PATH") {
        for path in std::env::split_paths(&env_paths) {
            if !path.as_os_str().is_empty() {
                push(path, ExtensionSource::Env);
            }
        }
    }

    if is_e2e_test_mode() {
        return paths;
    }

    if let Some(path) = builtin_plugins_dir() {
        push(path, ExtensionSource::Builtin);
    }
    push(data_dir.join("plugins"), ExtensionSource::Local);

    for legacy in resolve_scan_paths_for_data_dir(data_dir) {
        push(legacy.path, legacy.source);
    }
    paths
}

/// Persistent install target for user plugins.
pub fn resolve_plugin_install_target_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins")
}

fn builtin_plugins_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ELLM_SHA_BUILTIN_PLUGINS_PATH") {
        return Some(PathBuf::from(path));
    }

    // In repository/dev builds CARGO_MANIFEST_DIR is
    // `<v3>/aioncore/crates/aionui-extension`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(|root| root.join("plugins"))
}

/// Resolve the install target directory using the same priority order as
/// `resolve_scan_paths_for_data_dir`.
pub fn resolve_install_target_dir_for_data_dir(data_dir: &Path) -> PathBuf {
    resolve_scan_paths_for_data_dir(data_dir)
        .into_iter()
        .next()
        .map(|sp| sp.path)
        .unwrap_or_else(|| data_dir.join(EXTENSIONS_DIR_NAME))
}

/// Inner implementation that accepts explicit parameters for testability.
///
/// Production callers should use [`resolve_scan_paths`] which reads from
/// environment variables automatically.
fn resolve_scan_paths_inner(
    env_extensions_path: Option<&str>,
    e2e_mode: bool,
    explicit_data_dir: Option<&Path>,
) -> Vec<ScanPath> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut push = |path: PathBuf, source: ExtensionSource| {
        let normalized = path;
        if seen.insert(normalized.clone()) {
            paths.push(ScanPath {
                path: normalized,
                source,
            });
        }
    };

    // 1. Environment variable paths (highest priority).
    if let Some(env_paths) = env_extensions_path {
        for path in std::env::split_paths(env_paths) {
            if !path.as_os_str().is_empty() {
                push(path, ExtensionSource::Env);
            }
        }
    }

    // E2E test mode: only scan env var paths for isolation.
    if e2e_mode {
        return paths;
    }

    // 2. User data directory (desktop data dir or historical ~/.aionui fallback).
    if let Some(data_dir) = explicit_data_dir {
        push(data_dir.join(EXTENSIONS_DIR_NAME), ExtensionSource::Local);
        if let Some(appdata_dir) = derive_legacy_appdata_extensions_dir(data_dir) {
            push(appdata_dir, ExtensionSource::Appdata);
        }
    } else {
        if let Some(home) = dirs::home_dir() {
            push(home.join(".aionui").join(EXTENSIONS_DIR_NAME), ExtensionSource::Local);
        }

        // 3. AppData directory (platform-specific).
        if let Some(data_dir) = dirs::data_dir() {
            push(
                data_dir.join("aionui").join(EXTENSIONS_DIR_NAME),
                ExtensionSource::Appdata,
            );
        }
    }

    paths
}

fn derive_legacy_appdata_extensions_dir(data_dir: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let leaf = resolved.file_name()?.to_str()?;
    if leaf != "aionui" {
        return None;
    }
    Some(resolved.parent()?.join(EXTENSIONS_DIR_NAME))
}

// ---------------------------------------------------------------------------
// Extension loading
// ---------------------------------------------------------------------------

/// Scan all provided directories and load valid extension manifests.
///
/// When the same extension name appears in multiple scan paths, the first
/// occurrence wins (earlier entries have higher priority).
pub fn load_all(scan_paths: &[ScanPath]) -> Vec<LoadedExtension> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut result: Vec<LoadedExtension> = Vec::new();

    for sp in scan_paths {
        let loaded = scan_directory(&sp.path, sp.source);
        for ext in loaded {
            let name = ext.manifest.name.clone();
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(name.clone()) {
                e.insert(result.len());
                result.push(ext);
            } else {
                debug!(
                    name = %name,
                    skipped_path = %sp.path.display(),
                    "skipping duplicate extension (higher-priority copy already loaded)"
                );
            }
        }
    }

    result
}

/// Scan a single directory for extension subdirectories containing a
/// valid manifest file.
fn scan_directory(dir: &Path, source: ExtensionSource) -> Vec<LoadedExtension> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(dir = %dir.display(), error = %e, "failed to read extensions directory");
            }
            return Vec::new();
        }
    };

    let mut loaded = Vec::new();

    for entry in entries.flatten() {
        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with(".plugin-install-")
            || file_name.starts_with(".plugin-repository-")
            || file_name.starts_with(".plugin-backup-")
        {
            continue;
        }

        match load_plugin_directory(&entry_path, source) {
            Ok(ext) => {
                debug!(name = %ext.manifest.name, dir = %entry_path.display(), "loaded extension");
                loaded.push(ext);
            }
            Err(e) => {
                // Skip extensions with invalid manifests but continue loading others.
                warn!(
                    dir = %entry_path.display(),
                    error = %e,
                    "skipping extension with invalid manifest"
                );
            }
        }
    }

    loaded
}

/// Load a single extension from its manifest file.
pub(crate) fn load_single_extension(
    manifest_path: &Path,
    ext_dir: &Path,
    source: ExtensionSource,
) -> Result<LoadedExtension, crate::error::ExtensionError> {
    let bytes = std::fs::read(manifest_path)?;
    let manifest = parse_manifest_in_dir(&bytes, ext_dir)?;
    validate_extension_contribution_paths(&manifest, ext_dir)?;
    validate_mcp_server_configs(&manifest)?;

    let state = ExtensionState {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        enabled: true,
        installed_at: None,
        last_activated_at: None,
    };

    let directory = ext_dir.to_str().unwrap_or_default().to_owned();

    Ok(LoadedExtension {
        manifest,
        directory,
        source,
        state,
    })
}

/// Load either the native eLLMsha manifest or one of the explicitly supported
/// package conventions. Native manifests take precedence when both exist.
pub(crate) fn load_plugin_directory(
    ext_dir: &Path,
    source: ExtensionSource,
) -> Result<LoadedExtension, crate::error::ExtensionError> {
    let native_manifest = ext_dir.join(EXTENSION_MANIFEST_FILE);
    if native_manifest.is_file() {
        return load_single_extension(&native_manifest, ext_dir, source);
    }
    if !crate::compatibility::has_compatible_manifest(ext_dir) {
        return Err(crate::error::ExtensionError::ManifestValidation(
            "no recognized plugin manifest".into(),
        ));
    }
    let manifest = crate::compatibility::load_compatible_manifest(ext_dir)?;
    validate_manifest_runtime(&manifest, ext_dir, source)
}

fn validate_manifest_runtime(
    manifest: &crate::types::ExtensionManifest,
    ext_dir: &Path,
    source: ExtensionSource,
) -> Result<LoadedExtension, crate::error::ExtensionError> {
    crate::manifest::validate_manifest(manifest)?;
    validate_extension_contribution_paths(manifest, ext_dir)?;
    validate_mcp_server_configs(manifest)?;
    Ok(LoadedExtension {
        manifest: manifest.clone(),
        directory: ext_dir.to_string_lossy().into_owned(),
        source,
        state: ExtensionState {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            enabled: true,
            installed_at: None,
            last_activated_at: None,
        },
    })
}

fn validate_mcp_server_configs(manifest: &crate::types::ExtensionManifest) -> Result<(), crate::error::ExtensionError> {
    let Some(contributes) = &manifest.contributes else {
        return Ok(());
    };

    for server in &contributes.mcp_servers {
        let config = server.config.get("transport").unwrap_or(&server.config);
        if config.get("type").and_then(serde_json::Value::as_str) != Some("stdio") {
            continue;
        }
        let Some(command) = config.get("command").and_then(serde_json::Value::as_str) else {
            return Err(crate::error::ExtensionError::ManifestValidation(format!(
                "MCP server '{}' is missing a stdio command",
                server.id
            )));
        };
        let command_name = Path::new(command)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(command);
        if command_name != "node" && command_name != "node.exe" {
            continue;
        }
        let args = config
            .get("args")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>();
        let Some(eval_index) = args.iter().position(|arg| *arg == "-e" || *arg == "--eval") else {
            continue;
        };
        let Some(script) = args.get(eval_index + 1) else {
            return Err(crate::error::ExtensionError::ManifestValidation(format!(
                "MCP server '{}' is missing its Node eval script",
                server.id
            )));
        };

        let mut child = Command::new(command)
            .args(["--check", "--input-type=module"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                crate::error::ExtensionError::ManifestValidation(format!(
                    "MCP server '{}' Node syntax check could not start: {error}",
                    server.id
                ))
            })?;
        child.stdin.take().expect("piped stdin").write_all(script.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr.lines().next().unwrap_or("invalid JavaScript");
            return Err(crate::error::ExtensionError::ManifestValidation(format!(
                "MCP server '{}' has invalid Node eval script: {reason}",
                server.id
            )));
        }
    }

    Ok(())
}

pub(crate) fn validate_extension_contribution_paths(
    manifest: &crate::types::ExtensionManifest,
    ext_dir: &Path,
) -> Result<(), crate::error::ExtensionError> {
    let canonical_dir = std::fs::canonicalize(ext_dir)?;
    if let Some(contributes) = &manifest.contributes {
        for skill in &contributes.skills {
            let Some(relative_path) = &skill.path else {
                continue;
            };
            let resolved = std::fs::canonicalize(ext_dir.join(relative_path)).map_err(|_| {
                crate::error::ExtensionError::ManifestValidation(format!(
                    "skill '{}' path does not exist: {relative_path}",
                    skill.name
                ))
            })?;
            if !resolved.starts_with(&canonical_dir) {
                return Err(crate::error::ExtensionError::PathTraversal(relative_path.clone()));
            }
            crate::skill_service::validate_skill_file(&resolved)?;
        }
    }
    if let Some(hooks) = &manifest.lifecycle {
        for hook_path in [
            hooks.on_install.as_deref(),
            hooks.on_uninstall.as_deref(),
            hooks.on_activate.as_deref(),
            hooks.on_deactivate.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let declared = Path::new(hook_path);
            if declared.is_absolute()
                || declared.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir | std::path::Component::RootDir
                    )
                })
            {
                return Err(crate::error::ExtensionError::PathTraversal(hook_path.to_owned()));
            }
            let resolved = std::fs::canonicalize(ext_dir.join(hook_path)).map_err(|_| {
                crate::error::ExtensionError::ManifestValidation(format!(
                    "lifecycle hook path does not exist: {hook_path}"
                ))
            })?;
            if !resolved.starts_with(&canonical_dir) {
                return Err(crate::error::ExtensionError::PathTraversal(hook_path.to_owned()));
            }
            if !resolved.is_file() {
                return Err(crate::error::ExtensionError::ManifestValidation(format!(
                    "lifecycle hook path is not a file: {hook_path}"
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Engine compatibility filtering
// ---------------------------------------------------------------------------

/// Filter extensions by engine and API version compatibility.
///
/// Extensions that declare `engine.aionui` with a version range incompatible
/// with `app_version` are excluded. Extensions whose `apiVersion` is
/// incompatible with the supported [`EXTENSION_API_VERSION`] are also excluded.
///
/// Incompatible extensions are logged as warnings but do not cause errors.
pub fn filter_by_engine_compatibility(extensions: Vec<LoadedExtension>, app_version: &str) -> Vec<LoadedExtension> {
    let Ok(app_ver) = semver::Version::parse(app_version) else {
        warn!(
            app_version = %app_version,
            "invalid app version — skipping engine compatibility filter"
        );
        return extensions;
    };

    extensions
        .into_iter()
        .filter(|ext| is_engine_compatible(ext, &app_ver) && is_api_version_compatible(ext))
        .collect()
}

/// Check whether the extension's `engine.aionui` requirement is satisfied.
fn is_engine_compatible(ext: &LoadedExtension, app_version: &semver::Version) -> bool {
    let Some(engine) = &ext.manifest.engine else {
        return true; // no engine constraint
    };
    let Some(required) = &engine.aionui else {
        return true; // no aionui constraint
    };

    match semver::VersionReq::parse(required) {
        Ok(req) if req.matches(app_version) => true,
        Ok(_) => {
            warn!(
                name = %ext.manifest.name,
                required = %required,
                actual = %app_version,
                "extension filtered out: engine.aionui incompatible"
            );
            false
        }
        Err(e) => {
            warn!(
                name = %ext.manifest.name,
                required = %required,
                error = %e,
                "extension filtered out: invalid engine.aionui version requirement"
            );
            false
        }
    }
}

/// Check whether the extension's `apiVersion` is compatible with the
/// supported API version.
fn is_api_version_compatible(ext: &LoadedExtension) -> bool {
    let Some(api_ver_str) = &ext.manifest.api_version else {
        return true; // no API version constraint
    };

    let Ok(declared) = semver::Version::parse(api_ver_str) else {
        warn!(
            name = %ext.manifest.name,
            api_version = %api_ver_str,
            "extension filtered out: invalid apiVersion"
        );
        return false;
    };

    let Ok(supported) = semver::Version::parse(EXTENSION_API_VERSION) else {
        return true; // defensive — should never happen with a valid constant
    };

    // Compatible if major versions match and declared <= supported.
    if declared.major == supported.major && declared <= supported {
        true
    } else {
        warn!(
            name = %ext.manifest.name,
            declared = %declared,
            supported = %supported,
            "extension filtered out: apiVersion incompatible"
        );
        false
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_e2e_test_mode() -> bool {
    std::env::var("AIONUI_E2E_TEST").map(|v| v == "1").unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EngineConfig, ExtensionManifest};
    use std::fs;
    use tempfile::TempDir;

    /// Helper: create a minimal valid manifest JSON.
    fn write_manifest(dir: &Path, name: &str, version: &str) {
        write_manifest_full(dir, name, version, None, None);
    }

    /// Helper: create a manifest with optional engine and apiVersion fields.
    fn write_manifest_full(
        dir: &Path,
        name: &str,
        version: &str,
        engine_aionui: Option<&str>,
        api_version: Option<&str>,
    ) {
        let mut manifest = serde_json::json!({
            "name": name,
            "version": version,
        });
        if let Some(eng) = engine_aionui {
            manifest["engine"] = serde_json::json!({ "aionui": eng });
        }
        if let Some(api) = api_version {
            manifest["apiVersion"] = serde_json::json!(api);
        }
        let manifest_path = dir.join(EXTENSION_MANIFEST_FILE);
        fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }

    // -- scan_directory -------------------------------------------------------

    #[test]
    fn scan_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let result = scan_directory(tmp.path(), ExtensionSource::Local);
        assert!(result.is_empty());
    }

    #[test]
    fn scan_nonexistent_directory() {
        let result = scan_directory(Path::new("/nonexistent/path"), ExtensionSource::Local);
        assert!(result.is_empty());
    }

    #[test]
    fn scan_loads_valid_extension() {
        let tmp = TempDir::new().unwrap();
        let ext_dir = tmp.path().join("my-ext");
        fs::create_dir(&ext_dir).unwrap();
        write_manifest(&ext_dir, "my-ext", "1.0.0");

        let result = scan_directory(tmp.path(), ExtensionSource::Local);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].manifest.name, "my-ext");
        assert_eq!(result[0].manifest.version, "1.0.0");
        assert_eq!(result[0].source, ExtensionSource::Local);
        assert!(result[0].state.enabled);
    }

    #[test]
    fn scan_loads_aionui_main_contract_extension() {
        let tmp = TempDir::new().unwrap();
        let ext_dir = tmp.path().join("legacy-ext");
        fs::create_dir(&ext_dir).unwrap();
        fs::create_dir(ext_dir.join("contributes")).unwrap();

        fs::write(
            ext_dir.join("contributes/settings-tabs.json"),
            serde_json::to_vec_pretty(&serde_json::json!([
                {
                    "id": "legacy-settings",
                    "name": "Legacy Settings",
                    "entryPoint": "settings/legacy.html",
                    "position": { "anchor": "display", "placement": "after" }
                }
            ]))
            .unwrap(),
        )
        .unwrap();

        fs::write(
            ext_dir.join(EXTENSION_MANIFEST_FILE),
            serde_json::to_vec_pretty(&serde_json::json!({
                "name": "legacy-ext",
                "displayName": "Legacy Extension",
                "version": "1.0.0",
                "i18n": {
                    "localesDir": "i18n",
                    "defaultLocale": "en-US"
                },
                "contributes": {
                    "settingsTabs": "$file:contributes/settings-tabs.json",
                    "webui": {
                        "apiRoutes": [
                            {
                                "path": "/legacy-ext/collect",
                                "entryPoint": "webui/collector.js"
                            }
                        ],
                        "staticAssets": [
                            {
                                "urlPrefix": "/legacy-ext/assets",
                                "directory": "assets"
                            }
                        ]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let result = scan_directory(tmp.path(), ExtensionSource::Local);
        assert_eq!(result.len(), 1);
        let manifest = &result[0].manifest;
        assert_eq!(manifest.display_name.as_deref(), Some("Legacy Extension"));
        assert_eq!(manifest.i18n.as_ref().unwrap().locales, vec!["en-US".to_owned()]);
        assert_eq!(manifest.contributes.as_ref().unwrap().settings_tabs.len(), 1);
        assert_eq!(manifest.contributes.as_ref().unwrap().webui.len(), 2);
    }

    #[test]
    fn scan_skips_invalid_manifest() {
        let tmp = TempDir::new().unwrap();

        // Valid extension
        let good_dir = tmp.path().join("good-ext");
        fs::create_dir(&good_dir).unwrap();
        write_manifest(&good_dir, "good-ext", "1.0.0");

        // Invalid extension (bad JSON)
        let bad_dir = tmp.path().join("bad-ext");
        fs::create_dir(&bad_dir).unwrap();
        fs::write(bad_dir.join(EXTENSION_MANIFEST_FILE), b"not valid json").unwrap();

        let result = scan_directory(tmp.path(), ExtensionSource::Env);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].manifest.name, "good-ext");
    }

    #[test]
    fn scan_skips_directories_without_manifest() {
        let tmp = TempDir::new().unwrap();
        let ext_dir = tmp.path().join("no-manifest");
        fs::create_dir(&ext_dir).unwrap();
        fs::write(ext_dir.join("README.md"), b"hello").unwrap();

        let result = scan_directory(tmp.path(), ExtensionSource::Local);
        assert!(result.is_empty());
    }

    #[test]
    fn scan_skips_files_not_directories() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("not-a-dir.txt"), b"hello").unwrap();

        let result = scan_directory(tmp.path(), ExtensionSource::Local);
        assert!(result.is_empty());
    }

    #[test]
    fn scan_skips_plugin_install_transaction_directories() {
        let tmp = TempDir::new().unwrap();
        for name in [".plugin-install-stale", ".plugin-backup-stale"] {
            let dir = tmp.path().join(name);
            fs::create_dir(&dir).unwrap();
            write_manifest(&dir, "stale-transaction", "1.0.0");
        }

        let result = scan_directory(tmp.path(), ExtensionSource::Local);
        assert!(result.is_empty());
    }

    // -- load_all -------------------------------------------------------------

    #[test]
    fn load_all_deduplicates_by_name() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();

        // Same extension name in two directories
        let ext1 = tmp1.path().join("my-ext");
        fs::create_dir(&ext1).unwrap();
        write_manifest(&ext1, "my-ext", "1.0.0");

        let ext2 = tmp2.path().join("my-ext");
        fs::create_dir(&ext2).unwrap();
        write_manifest(&ext2, "my-ext", "2.0.0");

        let scan_paths = vec![
            ScanPath {
                path: tmp1.path().to_path_buf(),
                source: ExtensionSource::Env,
            },
            ScanPath {
                path: tmp2.path().to_path_buf(),
                source: ExtensionSource::Local,
            },
        ];

        let result = load_all(&scan_paths);
        assert_eq!(result.len(), 1);
        // First occurrence wins (higher priority).
        assert_eq!(result[0].manifest.version, "1.0.0");
        assert_eq!(result[0].source, ExtensionSource::Env);
    }

    #[test]
    fn load_all_from_multiple_directories() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();

        let ext1 = tmp1.path().join("ext-a");
        fs::create_dir(&ext1).unwrap();
        write_manifest(&ext1, "ext-a", "1.0.0");

        let ext2 = tmp2.path().join("ext-b");
        fs::create_dir(&ext2).unwrap();
        write_manifest(&ext2, "ext-b", "1.0.0");

        let scan_paths = vec![
            ScanPath {
                path: tmp1.path().to_path_buf(),
                source: ExtensionSource::Env,
            },
            ScanPath {
                path: tmp2.path().to_path_buf(),
                source: ExtensionSource::Local,
            },
        ];

        let result = load_all(&scan_paths);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn load_all_empty_paths() {
        let result = load_all(&[]);
        assert!(result.is_empty());
    }

    // -- filter_by_engine_compatibility ----------------------------------------

    fn make_loaded_ext(
        name: &str,
        version: &str,
        engine_aionui: Option<&str>,
        api_version: Option<&str>,
    ) -> LoadedExtension {
        LoadedExtension {
            manifest: ExtensionManifest {
                name: name.to_string(),
                version: version.to_string(),
                display_name: None,
                description: None,
                author: None,
                license: None,
                homepage: None,
                icon: None,
                engine: engine_aionui.map(|v| EngineConfig {
                    aionui: Some(v.to_string()),
                }),
                api_version: api_version.map(|v| v.to_string()),
                dependencies: HashMap::new(),
                entry_point: None,
                permissions: None,
                contributes: None,
                lifecycle: None,
                i18n: None,
                compatibility: None,
            },
            directory: format!("/test/{name}"),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: name.to_string(),
                version: version.to_string(),
                enabled: true,
                installed_at: None,
                last_activated_at: None,
            },
        }
    }

    #[test]
    fn filter_keeps_compatible_engine() {
        let exts = vec![make_loaded_ext("ext-a", "1.0.0", Some("^1.0.0"), None)];
        let filtered = filter_by_engine_compatibility(exts, "1.5.0");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_removes_incompatible_engine() {
        let exts = vec![make_loaded_ext("ext-a", "1.0.0", Some("^2.0.0"), None)];
        let filtered = filter_by_engine_compatibility(exts, "1.5.0");
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_keeps_no_engine_constraint() {
        let exts = vec![make_loaded_ext("ext-a", "1.0.0", None, None)];
        let filtered = filter_by_engine_compatibility(exts, "1.5.0");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_keeps_compatible_api_version() {
        let exts = vec![make_loaded_ext("ext-a", "1.0.0", None, Some("1.0.0"))];
        let filtered = filter_by_engine_compatibility(exts, "1.0.0");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_removes_incompatible_api_version() {
        // Extension requires API 2.0.0 but we support 1.0.0
        let exts = vec![make_loaded_ext("ext-a", "1.0.0", None, Some("2.0.0"))];
        let filtered = filter_by_engine_compatibility(exts, "1.0.0");
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_removes_invalid_engine_requirement() {
        let exts = vec![make_loaded_ext("ext-a", "1.0.0", Some("not-valid-semver-req"), None)];
        let filtered = filter_by_engine_compatibility(exts, "1.0.0");
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_keeps_all_with_invalid_app_version() {
        // If the app version itself is invalid, skip filtering entirely.
        let exts = vec![make_loaded_ext("ext-a", "1.0.0", Some("^2.0.0"), None)];
        let filtered = filter_by_engine_compatibility(exts, "not-semver");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_mixed_compatible_and_incompatible() {
        let exts = vec![
            make_loaded_ext("compatible", "1.0.0", Some("^1.0.0"), Some("1.0.0")),
            make_loaded_ext("bad-engine", "1.0.0", Some("^3.0.0"), None),
            make_loaded_ext("bad-api", "1.0.0", None, Some("2.0.0")),
            make_loaded_ext("no-constraint", "1.0.0", None, None),
        ];
        let filtered = filter_by_engine_compatibility(exts, "1.5.0");
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].manifest.name, "compatible");
        assert_eq!(filtered[1].manifest.name, "no-constraint");
    }

    // -- resolve_scan_paths_inner ------------------------------------------------

    #[test]
    fn resolve_scan_paths_includes_env_paths() {
        let paths = resolve_scan_paths_inner(Some("/tmp/test-exts"), false, None);
        assert!(
            paths
                .iter()
                .any(|sp| sp.path.as_path() == Path::new("/tmp/test-exts") && sp.source == ExtensionSource::Env)
        );
    }

    #[test]
    fn resolve_scan_paths_e2e_mode_only_env() {
        let paths = resolve_scan_paths_inner(Some("/tmp/e2e-exts"), true, None);
        assert!(paths.iter().all(|sp| sp.source == ExtensionSource::Env));
        assert!(paths.iter().any(|sp| sp.path.as_path() == Path::new("/tmp/e2e-exts")));
    }

    #[test]
    fn resolve_scan_paths_no_env_includes_platform_dirs() {
        let paths = resolve_scan_paths_inner(None, false, None);
        // Should have at least one platform dir (home or appdata).
        assert!(
            paths
                .iter()
                .any(|sp| sp.source == ExtensionSource::Local || sp.source == ExtensionSource::Appdata)
        );
    }

    #[test]
    fn resolve_scan_paths_e2e_no_env_returns_empty() {
        let paths = resolve_scan_paths_inner(None, true, None);
        assert!(paths.is_empty());
    }

    #[test]
    fn resolve_scan_paths_for_data_dir_prefers_env_then_data_dir_then_appdata() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app_root = tmp.path().join("AionUi-Dev");
        let data_dir = app_root.join("aionui");
        std::fs::create_dir_all(&data_dir).unwrap();
        let canonical_app_root = std::fs::canonicalize(&app_root).unwrap();

        let paths = resolve_scan_paths_inner(Some("/tmp/env-exts"), false, Some(&data_dir));
        assert_eq!(paths[0].path, PathBuf::from("/tmp/env-exts"));
        assert_eq!(paths[0].source, ExtensionSource::Env);
        assert_eq!(paths[1].path, data_dir.join(EXTENSIONS_DIR_NAME));
        assert_eq!(paths[1].source, ExtensionSource::Local);
        assert_eq!(paths[2].path, canonical_app_root.join(EXTENSIONS_DIR_NAME));
        assert_eq!(paths[2].source, ExtensionSource::Appdata);
    }

    #[test]
    fn resolve_scan_paths_for_data_dir_deduplicates_local_and_appdata() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("plain-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let paths = resolve_scan_paths_inner(None, false, Some(&data_dir));
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].path, data_dir.join(EXTENSIONS_DIR_NAME));
        assert_eq!(paths[0].source, ExtensionSource::Local);
    }
}
