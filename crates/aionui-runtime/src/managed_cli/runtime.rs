use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aionui_api_types::{CliRuntimeInfo, CliRuntimeSelection};
use fs2::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::cache;
use crate::node_runtime::ensure_node_runtime;
use crate::spawn::Builder;

use super::prepare::npm_package;
use super::{
    cli_version, current_runtime_key, main_executable_in, prepare_managed_cli_version_to_root, resolve_bundled_cli,
};

static VALIDATION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CliRuntimeError(String);

impl CliRuntimeError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SelectionState {
    mode: CliRuntimeSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    custom_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ManagedLocation {
    Bundled,
    Cache,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManagedRevision {
    version: String,
    location: ManagedLocation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct CliState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selection: Option<SelectionState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<ManagedRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous: Option<ManagedRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCliRuntime {
    pub path: PathBuf,
    pub selection: CliRuntimeSelection,
    pub selection_explicit: bool,
}

fn supported(name: &str) -> Result<&'static str, CliRuntimeError> {
    cli_version(name).ok_or_else(|| CliRuntimeError::new(format!("unsupported managed CLI '{name}'")))
}

fn cli_root() -> Result<PathBuf, CliRuntimeError> {
    cache::runtime_root()
        .map(|root| root.join("cli"))
        .ok_or_else(|| CliRuntimeError::new("managed CLI runtime root unavailable"))
}

fn state_path(root: &Path, name: &str) -> PathBuf {
    root.join(name).join("state.json")
}

fn lock_path(root: &Path, name: &str) -> PathBuf {
    root.join(name).join("state.lock")
}

fn read_state(root: &Path, name: &str) -> Result<CliState, CliRuntimeError> {
    let path = state_path(root, name);
    if !path.is_file() {
        return Ok(CliState::default());
    }
    let bytes = fs::read(&path).map_err(|error| CliRuntimeError::new(format!("read {}: {error}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|error| CliRuntimeError::new(format!("decode {}: {error}", path.display())))
}

fn write_state(root: &Path, name: &str, state: &CliState) -> Result<(), CliRuntimeError> {
    let parent = state_path(root, name)
        .parent()
        .ok_or_else(|| CliRuntimeError::new("managed CLI state path has no parent"))?
        .to_path_buf();
    fs::create_dir_all(&parent)
        .map_err(|error| CliRuntimeError::new(format!("create {}: {error}", parent.display())))?;
    let path = state_path(root, name);
    let temp = parent.join(format!("state.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| CliRuntimeError::new(format!("encode managed CLI state: {error}")))?;
    fs::write(&temp, bytes).map_err(|error| CliRuntimeError::new(format!("write {}: {error}", temp.display())))?;
    if cfg!(windows) && path.exists() {
        fs::remove_file(&path).map_err(|error| CliRuntimeError::new(format!("replace {}: {error}", path.display())))?;
    }
    fs::rename(&temp, &path).map_err(|error| CliRuntimeError::new(format!("activate {}: {error}", path.display())))
}

fn with_lock<T>(
    root: &Path,
    name: &str,
    operation: impl FnOnce() -> Result<T, CliRuntimeError>,
) -> Result<T, CliRuntimeError> {
    let lock = acquire_lock(root, name)?;
    let result = operation();
    let _ = FileExt::unlock(&lock);
    result
}

fn acquire_lock(root: &Path, name: &str) -> Result<std::fs::File, CliRuntimeError> {
    let path = lock_path(root, name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| CliRuntimeError::new(format!("create {}: {error}", parent.display())))?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| CliRuntimeError::new(format!("open {}: {error}", path.display())))?;
    FileExt::lock_exclusive(&lock)
        .map_err(|error| CliRuntimeError::new(format!("lock {}: {error}", path.display())))?;
    Ok(lock)
}

fn cached_executable(root: &Path, name: &str, version: &str) -> Result<PathBuf, CliRuntimeError> {
    let version_root = cached_version_root(root, name, version)?;
    main_executable_in(name, &version_root).ok_or_else(|| {
        CliRuntimeError::new(format!(
            "managed {name} {version} is missing or invalid under {}",
            version_root.display()
        ))
    })
}

fn cached_version_root(root: &Path, name: &str, version: &str) -> Result<PathBuf, CliRuntimeError> {
    let target = current_runtime_key().ok_or_else(|| CliRuntimeError::new("unsupported managed CLI platform"))?;
    Ok(root.join(name).join(version).join(target))
}

fn ensure_version_slot_available(path: &Path, name: &str, version: &str) -> Result<(), CliRuntimeError> {
    if path.exists() {
        return Err(CliRuntimeError::new(format!(
            "immutable managed {name} {version} slot already exists at {}",
            path.display()
        )));
    }
    Ok(())
}

fn resolve_revision(root: &Path, name: &str, revision: &ManagedRevision) -> Result<PathBuf, CliRuntimeError> {
    match revision.location {
        ManagedLocation::Bundled => {
            let bundled_version = supported(name)?;
            if revision.version != bundled_version {
                return Err(CliRuntimeError::new(format!(
                    "bundled {name} {} is no longer present; expected bundle version {bundled_version}",
                    revision.version
                )));
            }
            resolve_bundled_cli(name)
                .ok_or_else(|| CliRuntimeError::new(format!("bundled {name} {} is unavailable", revision.version)))
        }
        ManagedLocation::Cache => cached_executable(root, name, &revision.version),
    }
}

fn managed_revision(state: &CliState, name: &str) -> Result<ManagedRevision, CliRuntimeError> {
    Ok(state.active.clone().unwrap_or(ManagedRevision {
        version: supported(name)?.to_owned(),
        location: ManagedLocation::Bundled,
    }))
}

fn effective_selection(state: &CliState, legacy_custom_path: Option<&Path>) -> SelectionState {
    state.selection.clone().unwrap_or_else(|| {
        legacy_custom_path.map_or(
            SelectionState {
                mode: CliRuntimeSelection::Managed,
                custom_path: None,
            },
            |path| SelectionState {
                mode: CliRuntimeSelection::Custom,
                custom_path: Some(path.to_string_lossy().into_owned()),
            },
        )
    })
}

fn activate_revision(state: &mut CliState, name: &str, next: ManagedRevision) -> Result<(), CliRuntimeError> {
    let current = managed_revision(state, name)?;
    if current != next {
        state.previous = Some(current);
        state.active = Some(next);
    }
    Ok(())
}

fn apply_rollback(state: &mut CliState, name: &str, expected: &ManagedRevision) -> Result<(), CliRuntimeError> {
    let previous = state
        .previous
        .clone()
        .ok_or_else(|| CliRuntimeError::new(format!("no managed {name} rollback is available")))?;
    if &previous != expected {
        return Err(CliRuntimeError::new(format!(
            "managed {name} rollback changed while validating"
        )));
    }
    let current = managed_revision(state, name)?;
    state.active = Some(previous);
    state.previous = Some(current);
    Ok(())
}

fn resolve_from_root(
    root: &Path,
    name: &str,
    legacy_custom_path: Option<&Path>,
) -> Result<ResolvedCliRuntime, CliRuntimeError> {
    supported(name)?;
    let state = read_state(root, name)?;
    let selection_explicit = state.selection.is_some();
    let selection = effective_selection(&state, legacy_custom_path);
    let path = match selection.mode {
        CliRuntimeSelection::Managed => resolve_revision(root, name, &managed_revision(&state, name)?)?,
        CliRuntimeSelection::System => crate::resolve_command_path(name)
            .ok_or_else(|| CliRuntimeError::new(format!("system {name} was not found on PATH")))?,
        CliRuntimeSelection::Custom => {
            let raw = selection
                .custom_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| CliRuntimeError::new(format!("custom {name} path is empty")))?;
            let path = PathBuf::from(raw);
            if !path.is_file() {
                return Err(CliRuntimeError::new(format!(
                    "custom {name} path is unavailable: {}",
                    path.display()
                )));
            }
            path
        }
    };
    Ok(ResolvedCliRuntime {
        path,
        selection: selection.mode,
        selection_explicit,
    })
}

pub fn resolve_cli_runtime(
    name: &str,
    legacy_custom_path: Option<&Path>,
) -> Result<ResolvedCliRuntime, CliRuntimeError> {
    resolve_from_root(&cli_root()?, name, legacy_custom_path)
}

pub async fn set_cli_runtime_selection(
    name: &str,
    selection: CliRuntimeSelection,
    custom_path: Option<&str>,
) -> Result<(), CliRuntimeError> {
    supported(name)?;
    let custom_path = custom_path.map(str::trim).filter(|path| !path.is_empty());
    if selection == CliRuntimeSelection::Custom {
        let path = custom_path.ok_or_else(|| CliRuntimeError::new("custom runtime selection requires a path"))?;
        if !Path::new(path).is_file() {
            return Err(CliRuntimeError::new(format!(
                "custom {name} path is unavailable: {path}"
            )));
        }
    }
    let root = cli_root()?;
    let candidate = match selection {
        CliRuntimeSelection::Managed => {
            let state = read_state(&root, name)?;
            resolve_revision(&root, name, &managed_revision(&state, name)?)?
        }
        CliRuntimeSelection::System => crate::resolve_command_path(name)
            .ok_or_else(|| CliRuntimeError::new(format!("system {name} was not found on PATH")))?,
        CliRuntimeSelection::Custom => PathBuf::from(custom_path.unwrap()),
    };
    validate_cli(name, &candidate, None).await?;
    with_lock(&root, name, || {
        let mut state = read_state(&root, name)?;
        state.selection = Some(SelectionState {
            mode: selection,
            custom_path: (selection == CliRuntimeSelection::Custom).then(|| custom_path.unwrap().to_owned()),
        });
        write_state(&root, name, &state)
    })
}

async fn command_version(path: &Path) -> Result<String, CliRuntimeError> {
    let mut command = Builder::new(path);
    command.arg("--version");
    let output = command
        .output()
        .await
        .map_err(|error| CliRuntimeError::new(format!("run {} --version: {error}", path.display())))?;
    if !output.status.success() {
        return Err(CliRuntimeError::new(format!("{} --version failed", path.display())));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(if stdout.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    }
    .to_owned())
}

async fn validate_codex(path: &Path) -> Result<(), CliRuntimeError> {
    let schema_root = std::env::temp_dir().join(format!(
        "aionui-codex-schema-{}-{}",
        std::process::id(),
        VALIDATION_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&schema_root);
    fs::create_dir_all(&schema_root)
        .map_err(|error| CliRuntimeError::new(format!("create {}: {error}", schema_root.display())))?;
    let mut command = Builder::new(path);
    command.args(["app-server", "generate-json-schema", "--out"]);
    command.arg(&schema_root);
    let output = command
        .output()
        .await
        .map_err(|error| CliRuntimeError::new(format!("generate Codex app-server schema: {error}")))?;
    if !output.status.success() {
        let _ = fs::remove_dir_all(&schema_root);
        return Err(CliRuntimeError::new("Codex app-server schema generation failed"));
    }
    let mut schema = String::new();
    for entry in walkdir::WalkDir::new(&schema_root).into_iter().filter_map(Result::ok) {
        if entry.path().is_file() && entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
            if let Ok(text) = fs::read_to_string(entry.path()) {
                schema.push_str(&text);
            }
        }
    }
    let _ = fs::remove_dir_all(&schema_root);
    for method in [
        "model/list",
        "permissionProfile/list",
        "skills/extraRoots/set",
        "thread/start",
        "thread/resume",
        "thread/settings/update",
        "mcpServerStatus/list",
    ] {
        if !schema.contains(method) {
            return Err(CliRuntimeError::new(format!(
                "Codex app-server schema is missing {method}"
            )));
        }
    }
    Ok(())
}

async fn validate_claude(path: &Path) -> Result<(), CliRuntimeError> {
    let mut command = Builder::new(path);
    command.arg("--help");
    let output = command
        .output()
        .await
        .map_err(|error| CliRuntimeError::new(format!("run Claude --help: {error}")))?;
    if !output.status.success() {
        return Err(CliRuntimeError::new("Claude --help failed"));
    }
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for flag in ["--input-format", "--output-format", "--permission-mode", "--resume"] {
        if !help.contains(flag) {
            return Err(CliRuntimeError::new(format!(
                "Claude CLI is missing required flag {flag}"
            )));
        }
    }
    Ok(())
}

async fn validate_cli(name: &str, path: &Path, expected_version: Option<&str>) -> Result<String, CliRuntimeError> {
    let raw_version = command_version(path).await?;
    if let Some(expected) = expected_version
        && !raw_version
            .split_whitespace()
            .any(|token| token.trim_start_matches('v') == expected)
    {
        return Err(CliRuntimeError::new(format!(
            "{name} version mismatch: expected {expected}, got {raw_version}"
        )));
    }
    match name {
        "codex" => validate_codex(path).await?,
        "claude" => validate_claude(path).await?,
        _ => return Err(CliRuntimeError::new(format!("unsupported managed CLI '{name}'"))),
    }
    Ok(raw_version)
}

async fn latest_official_version(name: &str) -> Result<String, CliRuntimeError> {
    let package = npm_package(name).ok_or_else(|| CliRuntimeError::new(format!("unsupported managed CLI '{name}'")))?;
    let node = ensure_node_runtime()
        .await
        .map_err(|error| CliRuntimeError::new(format!("ensure Node runtime: {error}")))?;
    let cache = cli_root()?.join("npm-cache");
    fs::create_dir_all(&cache).map_err(|error| CliRuntimeError::new(format!("create {}: {error}", cache.display())))?;
    let mut command = Builder::from_resolved(&node.npm_command());
    command
        .env("npm_config_cache", &cache)
        .args(["view", package, "dist-tags.latest", "--json"]);
    let output = command
        .output()
        .await
        .map_err(|error| CliRuntimeError::new(format!("query {package} latest version: {error}")))?;
    if !output.status.success() {
        return Err(CliRuntimeError::new(format!("query {package} latest version failed")));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let version = raw.trim().trim_matches('"');
    let parsed = Version::parse(version)
        .map_err(|error| CliRuntimeError::new(format!("invalid official version '{version}': {error}")))?;
    if !parsed.pre.is_empty() || !parsed.build.is_empty() {
        return Err(CliRuntimeError::new(format!(
            "official latest {name} version is not a stable release: {version}"
        )));
    }
    Ok(version.to_owned())
}

fn validate_update_target(requested: Option<&str>, latest: &str, current: &str) -> Result<String, CliRuntimeError> {
    let latest_version = Version::parse(latest)
        .map_err(|error| CliRuntimeError::new(format!("invalid official version '{latest}': {error}")))?;
    if !latest_version.pre.is_empty() || !latest_version.build.is_empty() {
        return Err(CliRuntimeError::new(format!(
            "official latest version is not stable: {latest}"
        )));
    }
    let target = requested
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .unwrap_or(latest);
    let target_version =
        Version::parse(target).map_err(|error| CliRuntimeError::new(format!("invalid version '{target}': {error}")))?;
    if target_version != latest_version {
        return Err(CliRuntimeError::new(format!(
            "managed updates only accept the official stable latest version {latest}; requested {target}"
        )));
    }
    let current_version = Version::parse(current)
        .map_err(|error| CliRuntimeError::new(format!("invalid current version '{current}': {error}")))?;
    if target_version < current_version {
        return Err(CliRuntimeError::new(format!(
            "managed update would downgrade {current} to {target}"
        )));
    }
    Ok(target.to_owned())
}

fn bundled_runtime_root(name: &str, executable: &Path) -> Result<PathBuf, CliRuntimeError> {
    let parent_count = if name == "codex" { 4 } else { 1 };
    executable
        .ancestors()
        .nth(parent_count)
        .map(Path::to_path_buf)
        .ok_or_else(|| CliRuntimeError::new(format!("cannot locate bundled {name} runtime root")))
}

async fn preserve_bundled_revision(
    root: &Path,
    name: &str,
    revision: &ManagedRevision,
) -> Result<ManagedRevision, CliRuntimeError> {
    let final_root = cached_version_root(root, name, &revision.version)?;
    if final_root.exists() {
        let executable = main_executable_in(name, &final_root).ok_or_else(|| {
            CliRuntimeError::new(format!(
                "immutable managed {name} {} slot is invalid at {}",
                revision.version,
                final_root.display()
            ))
        })?;
        validate_cli(name, &executable, Some(&revision.version)).await?;
        return Ok(ManagedRevision {
            version: revision.version.clone(),
            location: ManagedLocation::Cache,
        });
    }
    let bundled_executable = resolve_revision(root, name, revision)?;
    validate_cli(name, &bundled_executable, Some(&revision.version)).await?;
    let source_root = bundled_runtime_root(name, &bundled_executable)?;
    let staging_parent = root.join(name).join(format!(
        ".preserve-{}-{}",
        std::process::id(),
        VALIDATION_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let staging_root = staging_parent
        .join(current_runtime_key().ok_or_else(|| CliRuntimeError::new("unsupported managed CLI platform"))?);
    crate::managed_resources::materialize_directory(&source_root, &staging_root)
        .map_err(|error| CliRuntimeError::new(format!("preserve bundled {name}: {error}")))?;
    let staged_executable = main_executable_in(name, &staging_root)
        .ok_or_else(|| CliRuntimeError::new(format!("preserved bundled {name} executable is missing")))?;
    if let Err(error) = validate_cli(name, &staged_executable, Some(&revision.version)).await {
        let _ = fs::remove_dir_all(&staging_parent);
        return Err(error);
    }
    if let Some(parent) = final_root.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| CliRuntimeError::new(format!("create {}: {error}", parent.display())))?;
    }
    if final_root.exists() {
        let _ = fs::remove_dir_all(&staging_parent);
        return Err(CliRuntimeError::new(format!(
            "immutable managed {name} {} slot appeared during preservation",
            revision.version
        )));
    }
    fs::rename(&staging_root, &final_root)
        .map_err(|error| CliRuntimeError::new(format!("preserve bundled {name}: {error}")))?;
    let _ = fs::remove_dir_all(&staging_parent);
    Ok(ManagedRevision {
        version: revision.version.clone(),
        location: ManagedLocation::Cache,
    })
}

pub async fn update_managed_cli(
    name: &str,
    requested_version: Option<&str>,
) -> Result<CliRuntimeInfo, CliRuntimeError> {
    supported(name)?;
    let root = cli_root()?;
    let update_lock = acquire_lock(&root, name)?;
    let before = read_state(&root, name)?;
    let current = managed_revision(&before, name)?;
    if let Some(version) = requested_version.map(str::trim).filter(|version| !version.is_empty()) {
        Version::parse(version)
            .map_err(|error| CliRuntimeError::new(format!("invalid version '{version}': {error}")))?;
    }
    let latest = latest_official_version(name).await?;
    let version = validate_update_target(requested_version, &latest, &current.version)?;
    if version == current.version {
        let _ = FileExt::unlock(&update_lock);
        return cli_runtime_info(name, None).await;
    }
    let next = ManagedRevision {
        version: version.clone(),
        location: ManagedLocation::Cache,
    };
    let executable = match cached_executable(&root, name, &version) {
        Ok(path) => {
            validate_cli(name, &path, Some(&version)).await?;
            path
        }
        Err(_) => {
            let final_root = cached_version_root(&root, name, &version)?;
            ensure_version_slot_available(&final_root, name, &version)?;
            let runtime_root = root
                .parent()
                .ok_or_else(|| CliRuntimeError::new("managed CLI root has no parent"))?;
            let staging_root = runtime_root.join(format!(".cli-stage-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&staging_root);
            let prepared = prepare_managed_cli_version_to_root(name, &version, &staging_root)
                .await
                .map_err(|error| CliRuntimeError::new(format!("stage {name} {version}: {error}")))?;
            let staged_executable = prepared.root.join(&prepared.executable);
            if let Err(error) = validate_cli(name, &staged_executable, Some(&version)).await {
                let _ = fs::remove_dir_all(&staging_root);
                return Err(error);
            }
            if let Some(parent) = final_root.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| CliRuntimeError::new(format!("create {}: {error}", parent.display())))?;
            }
            if let Err(error) = ensure_version_slot_available(&final_root, name, &version) {
                let _ = fs::remove_dir_all(&staging_root);
                return Err(error);
            }
            fs::rename(&prepared.root, &final_root).map_err(|error| {
                CliRuntimeError::new(format!(
                    "activate staged {name} {version} under {}: {error}",
                    final_root.display()
                ))
            })?;
            let _ = fs::remove_dir_all(&staging_root);
            main_executable_in(name, &final_root)
                .ok_or_else(|| CliRuntimeError::new(format!("activated {name} {version} executable is missing")))?
        }
    };
    validate_cli(name, &executable, Some(&version)).await?;
    let mut state = read_state(&root, name)?;
    let current = managed_revision(&state, name)?;
    if current.location == ManagedLocation::Bundled {
        state.active = Some(preserve_bundled_revision(&root, name, &current).await?);
    }
    let original = state.clone();
    activate_revision(&mut state, name, next)?;
    if state != original {
        write_state(&root, name, &state)?;
    }
    let _ = FileExt::unlock(&update_lock);
    cli_runtime_info(name, None).await
}

pub async fn rollback_managed_cli(name: &str) -> Result<CliRuntimeInfo, CliRuntimeError> {
    supported(name)?;
    let root = cli_root()?;
    let expected_previous = read_state(&root, name)?
        .previous
        .ok_or_else(|| CliRuntimeError::new(format!("no managed {name} rollback is available")))?;
    let path = resolve_revision(&root, name, &expected_previous)?;
    validate_cli(name, &path, Some(&expected_previous.version)).await?;
    with_lock(&root, name, || {
        let mut state = read_state(&root, name)?;
        apply_rollback(&mut state, name, &expected_previous)?;
        write_state(&root, name, &state)?;
        Ok(())
    })?;
    cli_runtime_info(name, None).await
}

pub async fn cli_runtime_info(
    name: &str,
    legacy_custom_path: Option<&Path>,
) -> Result<CliRuntimeInfo, CliRuntimeError> {
    let bundled_version = supported(name)?.to_owned();
    let root = cli_root()?;
    let state = read_state(&root, name)?;
    let resolution = resolve_from_root(&root, name, legacy_custom_path);
    let (selection, selection_explicit) = match &resolution {
        Ok(resolved) => (resolved.selection, resolved.selection_explicit),
        Err(_) => state.selection.as_ref().map_or(
            (
                if legacy_custom_path.is_some() {
                    CliRuntimeSelection::Custom
                } else {
                    CliRuntimeSelection::Managed
                },
                false,
            ),
            |selection| (selection.mode, true),
        ),
    };
    let (available, resolved_path, version, error) = match resolution {
        Ok(resolved) => match validate_cli(name, &resolved.path, None).await {
            Ok(version) => (
                true,
                Some(resolved.path.to_string_lossy().into_owned()),
                Some(version),
                None,
            ),
            Err(error) => (
                false,
                Some(resolved.path.to_string_lossy().into_owned()),
                None,
                Some(error.to_string()),
            ),
        },
        Err(error) => (false, None, None, Some(error.to_string())),
    };
    Ok(CliRuntimeInfo {
        name: name.to_owned(),
        selection,
        selection_explicit,
        available,
        resolved_path,
        version,
        bundled_version,
        active_managed_version: state.active.as_ref().map(|revision| revision.version.clone()),
        rollback_version: state.previous.as_ref().map(|revision| revision.version.clone()),
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(root: &Path, name: &str, version: &str) -> PathBuf {
        let target = current_runtime_key().unwrap();
        let version_root = root.join(name).join(version).join(target);
        let path = if name == "codex" {
            version_root.join("vendor/test-triple/bin/codex")
        } else {
            version_root.join("claude")
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"test").unwrap();
        path
    }

    #[test]
    fn explicit_selection_precedes_legacy_override() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let system_path = executable(root, "codex", "1.0.0");
        write_state(
            root,
            "codex",
            &CliState {
                selection: Some(SelectionState {
                    mode: CliRuntimeSelection::Custom,
                    custom_path: Some(system_path.to_string_lossy().into_owned()),
                }),
                ..CliState::default()
            },
        )
        .unwrap();
        let resolved = resolve_from_root(root, "codex", Some(Path::new("/legacy/codex"))).unwrap();
        assert_eq!(resolved.path, system_path);
        assert!(resolved.selection_explicit);
    }

    #[test]
    fn selection_precedence_is_explicit_then_legacy_then_managed() {
        let legacy = Path::new("/legacy/codex");
        let default = effective_selection(&CliState::default(), None);
        assert_eq!(default.mode, CliRuntimeSelection::Managed);

        let legacy_selection = effective_selection(&CliState::default(), Some(legacy));
        assert_eq!(legacy_selection.mode, CliRuntimeSelection::Custom);
        assert_eq!(legacy_selection.custom_path.as_deref(), Some("/legacy/codex"));

        let explicit = effective_selection(
            &CliState {
                selection: Some(SelectionState {
                    mode: CliRuntimeSelection::System,
                    custom_path: None,
                }),
                ..CliState::default()
            },
            Some(legacy),
        );
        assert_eq!(explicit.mode, CliRuntimeSelection::System);
        assert!(explicit.custom_path.is_none());
    }

    #[test]
    fn unavailable_custom_path_fails_without_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_state(
            root,
            "codex",
            &CliState {
                selection: Some(SelectionState {
                    mode: CliRuntimeSelection::Custom,
                    custom_path: Some("/missing/codex".into()),
                }),
                ..CliState::default()
            },
        )
        .unwrap();
        let error = resolve_from_root(root, "codex", None).unwrap_err();
        assert!(error.to_string().contains("unavailable"));
    }

    #[test]
    fn failed_staging_does_not_change_active_revision() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let state = CliState {
            active: Some(ManagedRevision {
                version: "1.0.0".into(),
                location: ManagedLocation::Cache,
            }),
            previous: Some(ManagedRevision {
                version: "0.9.0".into(),
                location: ManagedLocation::Cache,
            }),
            ..CliState::default()
        };
        write_state(root, "codex", &state).unwrap();
        assert!(cached_executable(root, "codex", "2.0.0").is_err());
        assert_eq!(read_state(root, "codex").unwrap(), state);
    }

    #[test]
    fn rollback_state_swaps_active_and_previous() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        executable(root, "codex", "1.0.0");
        executable(root, "codex", "2.0.0");
        let mut state = CliState {
            active: Some(ManagedRevision {
                version: "2.0.0".into(),
                location: ManagedLocation::Cache,
            }),
            previous: Some(ManagedRevision {
                version: "1.0.0".into(),
                location: ManagedLocation::Cache,
            }),
            ..CliState::default()
        };
        let previous = state.previous.clone().unwrap();
        resolve_revision(root, "codex", &previous).unwrap();
        apply_rollback(&mut state, "codex", &previous).unwrap();
        assert_eq!(state.active.as_ref().unwrap().version, "1.0.0");
        assert_eq!(state.previous.as_ref().unwrap().version, "2.0.0");
    }

    #[test]
    fn failed_rollback_validation_keeps_state_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let state = CliState {
            active: Some(ManagedRevision {
                version: "2.0.0".into(),
                location: ManagedLocation::Cache,
            }),
            previous: Some(ManagedRevision {
                version: "1.0.0".into(),
                location: ManagedLocation::Cache,
            }),
            ..CliState::default()
        };
        write_state(root, "codex", &state).unwrap();
        assert!(resolve_revision(root, "codex", state.previous.as_ref().unwrap()).is_err());
        assert_eq!(read_state(root, "codex").unwrap(), state);
    }

    #[test]
    fn resolved_path_is_pinned_for_existing_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let first_path = executable(root, "codex", "1.0.0");
        let second_path = executable(root, "codex", "2.0.0");
        let mut state = CliState {
            active: Some(ManagedRevision {
                version: "1.0.0".into(),
                location: ManagedLocation::Cache,
            }),
            ..CliState::default()
        };
        write_state(root, "codex", &state).unwrap();
        let existing_session_path = resolve_from_root(root, "codex", None).unwrap().path;
        activate_revision(
            &mut state,
            "codex",
            ManagedRevision {
                version: "2.0.0".into(),
                location: ManagedLocation::Cache,
            },
        )
        .unwrap();
        write_state(root, "codex", &state).unwrap();
        let new_session_path = resolve_from_root(root, "codex", None).unwrap().path;

        assert_eq!(existing_session_path, first_path);
        assert_eq!(new_session_path, second_path);
    }

    #[test]
    fn update_target_requires_latest_stable_without_downgrade() {
        assert_eq!(validate_update_target(None, "2.0.0", "1.0.0").unwrap(), "2.0.0");
        assert!(validate_update_target(Some("2.0.0-beta.1"), "2.0.0", "1.0.0").is_err());
        assert!(validate_update_target(Some("1.5.0"), "2.0.0", "1.0.0").is_err());
        assert!(validate_update_target(None, "1.0.0", "2.0.0").is_err());
    }

    #[test]
    fn immutable_version_slot_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("codex/2.0.0/target");
        fs::create_dir_all(&slot).unwrap();
        let marker = slot.join("owned-by-running-session");
        fs::write(&marker, b"keep").unwrap();

        let error = ensure_version_slot_available(&slot, "codex", "2.0.0").unwrap_err();

        assert!(error.to_string().contains("immutable"));
        assert_eq!(fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn bundled_revision_must_match_current_bundle_version() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_revision(
            dir.path(),
            "codex",
            &ManagedRevision {
                version: "0.0.1".into(),
                location: ManagedLocation::Bundled,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("no longer present"));
    }
}
