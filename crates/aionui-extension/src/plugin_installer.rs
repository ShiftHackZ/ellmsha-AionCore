use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::constants::EXTENSION_MANIFEST_FILE;
use crate::dependency::validate_dependencies;
use crate::error::ExtensionError;
use crate::loader::{filter_by_engine_compatibility, load_plugin_directory};
use crate::registry::ExtensionRegistry;
use crate::resolvers::resolve_extension_contributions;
use crate::types::{ExtensionSource, ExtensionState, LoadedExtension};

/// Supported plugin installation sources.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginInstallSource {
    LocalPath {
        path: PathBuf,
    },
    Git {
        url: String,
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
        #[serde(default)]
        subdirectory: Option<PathBuf>,
    },
}

const PLUGIN_PROVENANCE_FILE: &str = ".ellmsha-plugin-source.json";
const MAX_ARCHIVE_FILES: usize = 10_000;
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginInstallProvenance {
    LocalPath {
        path: PathBuf,
    },
    Git {
        url: String,
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subdirectory: Option<PathBuf>,
        resolved_revision: String,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub name: String,
    pub version: String,
    pub directory: PathBuf,
    pub provenance: PluginInstallProvenance,
}

/// Installs validated plugins into the persistent userData plugin directory.
/// A candidate is staged and validated before the currently working version is
/// moved, so a failed install/update leaves that version untouched.
#[derive(Clone)]
pub struct PluginInstaller {
    target_root: PathBuf,
    registry: ExtensionRegistry,
    install_lock: Arc<tokio::sync::Mutex<()>>,
}

impl PluginInstaller {
    pub fn new(target_root: PathBuf, registry: ExtensionRegistry) -> Self {
        Self {
            target_root,
            registry,
            install_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub async fn install(&self, source: PluginInstallSource) -> Result<InstalledPlugin, ExtensionError> {
        let _guard = self.install_lock.lock().await;
        self.install_locked(source, None).await
    }

    async fn install_locked(
        &self,
        source: PluginInstallSource,
        expected_name: Option<String>,
    ) -> Result<InstalledPlugin, ExtensionError> {
        let target_root = self.target_root.clone();
        let (app_version, installed_extensions) = self.registry.install_validation_context().await;
        let installed = tokio::task::spawn_blocking(move || {
            install_blocking(
                &target_root,
                source,
                expected_name.as_deref(),
                &app_version,
                installed_extensions,
            )
        })
        .await
        .map_err(|error| ExtensionError::Internal(format!("plugin install task failed: {error}")))??;
        self.registry.hot_reload().await;
        Ok(installed)
    }

    /// Refresh a user plugin from its recorded source while preserving the
    /// selected Git ref and repository subdirectory.
    pub async fn update(&self, name: &str) -> Result<InstalledPlugin, ExtensionError> {
        let _guard = self.install_lock.lock().await;
        let provenance = read_installed_provenance(&self.target_root, name)?.ok_or_else(|| {
            ExtensionError::InvalidRequest(format!(
                "plugin '{name}' has no recorded install source; reinstall it to enable updates"
            ))
        })?;
        self.install_locked(provenance.into_source(), Some(name.to_owned()))
            .await
    }

    pub async fn provenance(&self, name: &str) -> Result<Option<PluginInstallProvenance>, ExtensionError> {
        let _guard = self.install_lock.lock().await;
        read_installed_provenance(&self.target_root, name)
    }

    /// Remove a persistent user plugin and propagate the registry generation.
    pub async fn uninstall(&self, name: &str) -> Result<(), ExtensionError> {
        let _guard = self.install_lock.lock().await;
        validate_plugin_name(name)?;
        let extension = self
            .registry
            .get_extension_by_name(name)
            .await
            .ok_or_else(|| ExtensionError::NotFound(name.to_owned()))?;
        if extension.source != ExtensionSource::Local {
            return Err(ExtensionError::InvalidRequest(format!(
                "plugin '{name}' is not a removable user plugin"
            )));
        }
        let target_root = std::fs::canonicalize(&self.target_root)?;
        let target = std::fs::canonicalize(self.target_root.join(name))?;
        if target.parent() != Some(target_root.as_path())
            || target.file_name().and_then(|part| part.to_str()) != Some(name)
        {
            return Err(ExtensionError::PathTraversal(target.to_string_lossy().into_owned()));
        }
        let backup = self
            .target_root
            .join(format!(".plugin-backup-{name}-{}", aionui_common::now_ms()));
        std::fs::rename(&target, &backup)?;
        self.registry.hot_reload().await;
        if self.registry.get_extension_by_name(name).await.is_some() {
            let _ = std::fs::rename(&backup, &target);
            self.registry.hot_reload().await;
            return Err(ExtensionError::Internal(format!(
                "plugin '{name}' remained loaded after removal"
            )));
        }
        std::fs::remove_dir_all(&backup)?;
        Ok(())
    }
}

impl PluginInstallProvenance {
    fn into_source(self) -> PluginInstallSource {
        match self {
            Self::LocalPath { path } => PluginInstallSource::LocalPath { path },
            Self::Git {
                url,
                git_ref,
                subdirectory,
                ..
            } => PluginInstallSource::Git {
                url,
                git_ref,
                subdirectory,
            },
        }
    }
}

fn install_blocking(
    target_root: &Path,
    source: PluginInstallSource,
    expected_name: Option<&str>,
    app_version: &str,
    installed_extensions: Vec<LoadedExtension>,
) -> Result<InstalledPlugin, ExtensionError> {
    std::fs::create_dir_all(target_root)?;
    let canonical_target_root = std::fs::canonicalize(target_root)?;
    let nonce = format!("{}-{}", std::process::id(), aionui_common::now_ms());
    let staging = target_root.join(format!(".plugin-install-{nonce}"));
    let repository_staging = target_root.join(format!(".plugin-repository-{nonce}"));

    let stage_result: Result<PluginInstallProvenance, ExtensionError> = match source {
        PluginInstallSource::LocalPath { path } => {
            let canonical_source = std::fs::canonicalize(&path).map_err(|error| {
                ExtensionError::InvalidRequest(format!("plugin source {} is unavailable: {error}", path.display()))
            })?;
            if canonical_target_root.starts_with(&canonical_source) {
                return Err(ExtensionError::InvalidRequest(
                    "plugin source must not contain the persistent plugin directory".into(),
                ));
            }
            copy_plugin_tree(&canonical_source, &staging)
                .map(|()| PluginInstallProvenance::LocalPath { path: canonical_source })
        }
        PluginInstallSource::Git {
            url,
            git_ref,
            subdirectory,
        } => stage_git_plugin(
            &url,
            git_ref.as_deref(),
            subdirectory.as_deref(),
            &repository_staging,
            &staging,
        ),
    };
    let provenance = match stage_result {
        Ok(provenance) => provenance,
        Err(error) => {
            cleanup_staging(&staging, &repository_staging);
            return Err(error);
        }
    };
    let _ = std::fs::remove_dir_all(&repository_staging);

    if let Err(error) = write_provenance(&staging, &provenance) {
        cleanup_staging(&staging, &repository_staging);
        return Err(error);
    }

    let manifest = match validate_candidate(&staging) {
        Ok(manifest) => manifest,
        Err(error) => {
            cleanup_staging(&staging, &repository_staging);
            return Err(error);
        }
    };
    if let Err(error) = validate_plugin_name(&manifest.name) {
        cleanup_staging(&staging, &repository_staging);
        return Err(error);
    }
    if let Some(expected) = expected_name.filter(|expected| *expected != manifest.name) {
        cleanup_staging(&staging, &repository_staging);
        return Err(ExtensionError::InvalidRequest(format!(
            "plugin update expected '{expected}', but the selected source now declares '{}'",
            manifest.name
        )));
    }
    if let Err(error) = validate_candidate_compatibility(&manifest, &staging, app_version, installed_extensions) {
        cleanup_staging(&staging, &repository_staging);
        return Err(error);
    }

    let target = target_root.join(&manifest.name);
    let backup = target_root.join(format!(".plugin-backup-{}-{nonce}", manifest.name));
    let had_previous = target.exists();
    if had_previous {
        if let Err(error) = std::fs::rename(&target, &backup) {
            cleanup_staging(&staging, &repository_staging);
            return Err(error.into());
        }
    }

    if let Err(error) = std::fs::rename(&staging, &target) {
        if had_previous {
            let _ = std::fs::rename(&backup, &target);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(ExtensionError::Io(error));
    }

    if had_previous {
        let _ = std::fs::remove_dir_all(&backup);
    }

    Ok(InstalledPlugin {
        name: manifest.name,
        version: manifest.version,
        directory: target,
        provenance,
    })
}

fn cleanup_staging(staging: &Path, repository_staging: &Path) {
    let _ = std::fs::remove_dir_all(staging);
    let _ = std::fs::remove_dir_all(repository_staging);
}

fn write_provenance(directory: &Path, provenance: &PluginInstallProvenance) -> Result<(), ExtensionError> {
    let bytes = serde_json::to_vec_pretty(provenance)
        .map_err(|error| ExtensionError::Internal(format!("failed to serialize plugin provenance: {error}")))?;
    std::fs::write(directory.join(PLUGIN_PROVENANCE_FILE), bytes)?;
    Ok(())
}

fn read_installed_provenance(
    target_root: &Path,
    name: &str,
) -> Result<Option<PluginInstallProvenance>, ExtensionError> {
    validate_plugin_name(name)?;
    let canonical_root = std::fs::canonicalize(target_root)?;
    let target = match std::fs::canonicalize(target_root.join(name)) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if target.parent() != Some(canonical_root.as_path())
        || target.file_name().and_then(|part| part.to_str()) != Some(name)
    {
        return Err(ExtensionError::PathTraversal(target.to_string_lossy().into_owned()));
    }
    let path = target.join(PLUGIN_PROVENANCE_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| ExtensionError::InvalidRequest(format!("plugin provenance is invalid: {error}")))
}

fn validate_candidate_compatibility(
    manifest: &crate::types::ExtensionManifest,
    staging: &Path,
    app_version: &str,
    mut installed: Vec<LoadedExtension>,
) -> Result<(), ExtensionError> {
    if installed.iter().any(|extension| {
        extension.manifest.name == manifest.name
            && matches!(extension.source, ExtensionSource::Env | ExtensionSource::Builtin)
    }) {
        return Err(ExtensionError::InvalidRequest(format!(
            "plugin '{}' is provided by a higher-priority source",
            manifest.name
        )));
    }

    let baseline_issues: std::collections::HashSet<String> = validate_dependencies(&installed)
        .issues
        .into_iter()
        .map(|issue| format!("{issue:?}"))
        .collect();
    installed.retain(|extension| extension.manifest.name != manifest.name);
    installed.push(LoadedExtension {
        state: ExtensionState {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            enabled: true,
            installed_at: None,
            last_activated_at: None,
        },
        manifest: manifest.clone(),
        directory: staging.to_string_lossy().into_owned(),
        source: ExtensionSource::Local,
    });

    let compatible = filter_by_engine_compatibility(installed.clone(), app_version);
    if !compatible
        .iter()
        .any(|extension| extension.manifest.name == manifest.name)
    {
        return Err(ExtensionError::ManifestValidation(format!(
            "plugin '{}' is incompatible with this eLLMsha runtime",
            manifest.name
        )));
    }
    let new_issues: Vec<String> = validate_dependencies(&compatible)
        .issues
        .into_iter()
        .map(|issue| format!("{issue:?}"))
        .filter(|issue| !baseline_issues.contains(issue))
        .collect();
    if !new_issues.is_empty() {
        return Err(ExtensionError::ManifestValidation(format!(
            "plugin '{}' introduces dependency errors: {}",
            manifest.name,
            new_issues.join(", ")
        )));
    }
    Ok(())
}

fn validate_candidate(directory: &Path) -> Result<crate::types::ExtensionManifest, ExtensionError> {
    let loaded = load_plugin_directory(directory, ExtensionSource::Local).map_err(|error| {
        ExtensionError::ManifestValidation(format!(
            "plugin candidate at {} is invalid: {error}",
            directory.display()
        ))
    })?;
    let manifest = loaded.manifest.clone();
    let _ = resolve_extension_contributions(&loaded);
    Ok(manifest)
}

fn validate_plugin_name(name: &str) -> Result<(), ExtensionError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ExtensionError::InvalidRequest(format!("invalid plugin name '{name}'")));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedGitSource {
    url: String,
    git_ref: Option<String>,
    subdirectory: Option<PathBuf>,
}

fn stage_git_plugin(
    url: &str,
    git_ref: Option<&str>,
    subdirectory: Option<&Path>,
    repository_staging: &Path,
    plugin_staging: &Path,
) -> Result<PluginInstallProvenance, ExtensionError> {
    let source = normalize_git_source(url, git_ref, subdirectory)?;
    validate_git_url(&source.url)?;
    if let Some(git_ref) = source.git_ref.as_deref() {
        validate_git_ref(git_ref)?;
    }
    if let Some(subdirectory) = source.subdirectory.as_deref() {
        validate_subdirectory(subdirectory)?;
    }

    let output = Command::new("git")
        .args(["init", "--bare", "--"])
        .arg(repository_staging)
        .output()
        .map_err(|error| ExtensionError::Internal(format!("failed to launch git: {error}")))?;
    if !output.status.success() {
        return Err(ExtensionError::InvalidRequest(format!(
            "git repository initialization failed with status {}",
            output.status
        )));
    }

    let requested_revision = source.git_ref.as_deref().unwrap_or("HEAD");
    let fetch_output = Command::new("git")
        .current_dir(repository_staging)
        .args(["fetch", "--depth=1", "--filter=blob:none", "--no-tags"])
        .arg(&source.url)
        .arg(requested_revision)
        .output()
        .map_err(|error| ExtensionError::Internal(format!("failed to fetch git source: {error}")))?;
    if !fetch_output.status.success() {
        return Err(ExtensionError::InvalidRequest(format!(
            "git fetch failed for ref '{}' with status {}",
            requested_revision, fetch_output.status
        )));
    }

    let revision_output = Command::new("git")
        .current_dir(repository_staging)
        .args(["rev-parse", "--verify", "--end-of-options"])
        .arg("FETCH_HEAD^{commit}")
        .output()
        .map_err(|error| ExtensionError::Internal(format!("failed to resolve git ref: {error}")))?;
    if !revision_output.status.success() {
        return Err(ExtensionError::InvalidRequest(format!(
            "git ref '{}' was not found in the repository",
            requested_revision
        )));
    }
    let resolved_revision = String::from_utf8_lossy(&revision_output.stdout).trim().to_owned();
    if !is_full_revision(&resolved_revision) {
        return Err(ExtensionError::InvalidRequest(
            "git returned an invalid resolved revision".into(),
        ));
    }

    if let Some(subdirectory) = source.subdirectory.as_deref() {
        let tree_expression = format!("{resolved_revision}:{}", git_path(subdirectory));
        let tree_output = Command::new("git")
            .current_dir(repository_staging)
            .args(["cat-file", "-t", "--"])
            .arg(&tree_expression)
            .output()
            .map_err(|error| ExtensionError::Internal(format!("failed to inspect git subdirectory: {error}")))?;
        if !tree_output.status.success() || String::from_utf8_lossy(&tree_output.stdout).trim() != "tree" {
            return Err(ExtensionError::InvalidRequest(format!(
                "git subdirectory '{}' does not exist at revision {} or is not a directory",
                subdirectory.display(),
                resolved_revision
            )));
        }
    }

    let mut archive_command = Command::new("git");
    archive_command
        .current_dir(repository_staging)
        .args(["archive", "--format=zip"])
        .arg(&resolved_revision);
    if let Some(subdirectory) = source.subdirectory.as_deref() {
        archive_command.arg("--").arg(git_path(subdirectory));
    }
    let archive = run_git_archive_bounded(archive_command, MAX_ARCHIVE_BYTES)?;
    extract_plugin_archive(&archive, source.subdirectory.as_deref(), plugin_staging)?;

    Ok(PluginInstallProvenance::Git {
        url: source.url,
        git_ref: source.git_ref,
        subdirectory: source.subdirectory,
        resolved_revision,
    })
}

fn normalize_git_source(
    raw_url: &str,
    explicit_ref: Option<&str>,
    explicit_subdirectory: Option<&Path>,
) -> Result<NormalizedGitSource, ExtensionError> {
    let raw_url = raw_url.trim();
    if raw_url.is_empty() {
        return Err(ExtensionError::InvalidRequest("git URL must not be empty".into()));
    }
    let mut normalized = NormalizedGitSource {
        url: raw_url.to_owned(),
        git_ref: trimmed_option(explicit_ref),
        subdirectory: explicit_subdirectory
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf),
    };

    let Ok(parsed) = url::Url::parse(raw_url) else {
        return Ok(normalized);
    };
    if parsed.host_str() != Some("github.com") {
        return Ok(normalized);
    }
    let segments: Vec<&str> = parsed
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() < 4 || segments[2] != "tree" {
        return Ok(normalized);
    }
    if parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(ExtensionError::InvalidRequest(
            "GitHub tree URLs must not contain credentials, query parameters, or fragments".into(),
        ));
    }
    let inferred_ref = segments[3];
    if !is_full_revision(inferred_ref) {
        return Err(ExtensionError::InvalidRequest(
            "GitHub tree URLs with branch or tag names are ambiguous; use the repository root URL with explicit ref and subdirectory fields".into(),
        ));
    }
    if segments.len() == 4 {
        return Err(ExtensionError::InvalidRequest(
            "GitHub tree URL must select a plugin subdirectory".into(),
        ));
    }
    let inferred_subdirectory = PathBuf::from(segments[4..].join("/"));
    if normalized.git_ref.as_deref().is_some_and(|value| value != inferred_ref)
        || normalized
            .subdirectory
            .as_deref()
            .is_some_and(|value| value != inferred_subdirectory)
    {
        return Err(ExtensionError::InvalidRequest(
            "Git URL selection conflicts with the explicit ref or subdirectory".into(),
        ));
    }
    let repository = segments[1].trim_end_matches(".git");
    normalized.url = format!("https://github.com/{}/{}.git", segments[0], repository);
    normalized.git_ref = Some(inferred_ref.to_owned());
    normalized.subdirectory = Some(inferred_subdirectory);
    Ok(normalized)
}

fn trimmed_option(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn validate_git_url(url: &str) -> Result<(), ExtensionError> {
    if url.starts_with('-') || url.chars().any(char::is_control) {
        return Err(ExtensionError::InvalidRequest("git URL is invalid".into()));
    }
    if Path::new(url).is_absolute() || Path::new(url).exists() {
        return Ok(());
    }
    if let Ok(parsed) = url::Url::parse(url) {
        if !matches!(parsed.scheme(), "http" | "https" | "ssh" | "git" | "file") {
            return Err(ExtensionError::InvalidRequest(format!(
                "unsupported git URL scheme '{}'",
                parsed.scheme()
            )));
        }
        if parsed.password().is_some() || (matches!(parsed.scheme(), "http" | "https") && !parsed.username().is_empty())
        {
            return Err(ExtensionError::InvalidRequest(
                "git URLs must not contain embedded credentials".into(),
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(ExtensionError::InvalidRequest(
                "git URLs must not contain query parameters or fragments".into(),
            ));
        }
    }
    Ok(())
}

fn validate_git_ref(git_ref: &str) -> Result<(), ExtensionError> {
    if git_ref.is_empty() || git_ref.starts_with('-') || git_ref.chars().any(char::is_control) {
        return Err(ExtensionError::InvalidRequest("git ref is invalid".into()));
    }
    Ok(())
}

fn validate_subdirectory(path: &Path) -> Result<(), ExtensionError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.to_string_lossy().contains('\\')
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ExtensionError::InvalidRequest(format!(
            "git subdirectory must be a confined relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn git_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn is_full_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn run_git_archive_bounded(mut command: Command, max_bytes: u64) -> Result<Vec<u8>, ExtensionError> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| ExtensionError::Internal(format!("failed to export git plugin: {error}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExtensionError::Internal("git archive did not provide a stdout stream".into()))?;
    let mut archive = Vec::new();
    stdout.by_ref().take(max_bytes + 1).read_to_end(&mut archive)?;
    if archive.len() as u64 > max_bytes {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ExtensionError::InvalidRequest(format!(
            "git plugin archive exceeds {max_bytes} bytes"
        )));
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(ExtensionError::InvalidRequest(format!(
            "git archive failed with status {status}"
        )));
    }
    Ok(archive)
}

fn extract_plugin_archive(
    archive: &[u8],
    subdirectory: Option<&Path>,
    destination: &Path,
) -> Result<(), ExtensionError> {
    let cursor = Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|error| ExtensionError::InvalidRequest(format!("git archive is invalid: {error}")))?;
    if zip.len() > MAX_ARCHIVE_FILES {
        return Err(ExtensionError::InvalidRequest(format!(
            "git plugin archive contains too many files (maximum {MAX_ARCHIVE_FILES})"
        )));
    }
    std::fs::create_dir_all(destination)?;
    let mut total_bytes = 0_u64;
    let mut extracted_files = 0_usize;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| ExtensionError::InvalidRequest(format!("git archive entry is invalid: {error}")))?;
        total_bytes = total_bytes.saturating_add(entry.size());
        if total_bytes > MAX_ARCHIVE_BYTES {
            return Err(ExtensionError::InvalidRequest(format!(
                "git plugin archive exceeds {} bytes",
                MAX_ARCHIVE_BYTES
            )));
        }
        let enclosed = entry.enclosed_name().ok_or_else(|| {
            ExtensionError::InvalidRequest(format!("git archive contains an escaping path: {}", entry.name()))
        })?;
        let relative = match subdirectory {
            Some(prefix) => match enclosed.strip_prefix(prefix) {
                Ok(relative) => relative,
                Err(_) if entry.is_dir() && prefix.starts_with(&enclosed) => continue,
                Err(_) => {
                    return Err(ExtensionError::InvalidRequest(format!(
                        "git archive entry is outside selected subdirectory: {}",
                        enclosed.display()
                    )));
                }
            },
            None => enclosed.as_path(),
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        validate_subdirectory(relative)?;
        let mode = entry.unix_mode().unwrap_or(0o100644);
        let kind = mode & 0o170000;
        if kind == 0o120000 {
            return Err(ExtensionError::InvalidRequest(format!(
                "plugin packages may not contain symlinks: {}",
                enclosed.display()
            )));
        }
        if kind != 0 && kind != 0o040000 && kind != 0o100000 {
            return Err(ExtensionError::InvalidRequest(format!(
                "plugin archive contains an unsupported file type: {}",
                enclosed.display()
            )));
        }
        let target = destination.join(relative);
        if entry.is_dir() || kind == 0o040000 {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&target)?;
        std::io::copy(&mut entry.by_ref().take(MAX_ARCHIVE_BYTES + 1), &mut output)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode & 0o777))?;
        }
        extracted_files += 1;
    }
    if extracted_files == 0 {
        return Err(ExtensionError::InvalidRequest(
            "selected git plugin subdirectory is empty".into(),
        ));
    }
    Ok(())
}

pub(crate) fn copy_plugin_tree(source: &Path, destination: &Path) -> Result<(), ExtensionError> {
    let source = std::fs::canonicalize(source).map_err(|error| {
        ExtensionError::InvalidRequest(format!("plugin source {} is unavailable: {error}", source.display()))
    })?;
    if !source.is_dir() {
        return Err(ExtensionError::InvalidRequest(format!(
            "plugin source {} is not a directory",
            source.display()
        )));
    }
    copy_directory(&source, destination)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), ExtensionError> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let target = destination.join(&name);
        if file_type.is_symlink() {
            return Err(ExtensionError::InvalidRequest(format!(
                "plugin packages may not contain symlinks: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::BroadcastEventBus;
    use std::sync::Arc;

    fn registry(root: &Path) -> ExtensionRegistry {
        let state = crate::ExtensionStateStore::new(root.join("states.json"));
        ExtensionRegistry::new(state, Arc::new(BroadcastEventBus::new(16)), "1.0.0".into())
    }

    fn fixture(root: &Path, version: &str) {
        std::fs::create_dir_all(root.join("skills/safe")).unwrap();
        std::fs::write(
            root.join("skills/safe/SKILL.md"),
            "---\nname: safe\ndescription: safe\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join(EXTENSION_MANIFEST_FILE),
            serde_json::to_vec(&serde_json::json!({
                "name": "fixture-plugin",
                "version": version,
                "contributes": { "skills": [{ "name": "safe", "path": "skills/safe/SKILL.md" }] }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn rename_fixture(root: &Path, name: &str, version: &str) {
        let path = root.join(EXTENSION_MANIFEST_FILE);
        let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["name"] = serde_json::Value::String(name.to_owned());
        manifest["version"] = serde_json::Value::String(version.to_owned());
        std::fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }

    fn add_node_mcp(root: &Path, script: &str) {
        std::fs::write(root.join("server.mjs"), script).unwrap();
        let manifest_path = root.join(EXTENSION_MANIFEST_FILE);
        let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["contributes"]["mcpServers"] = serde_json::json!([{
            "id": "safe",
            "name": "Safe",
            "transport": {
                "type": "stdio",
                "command": "node",
                "args": ["-e", "$file:server.mjs"]
            }
        }]);
        std::fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }

    fn git(source: &Path, args: &[&str]) -> String {
        let output = Command::new("git").current_dir(source).args(args).output().unwrap();
        assert!(output.status.success(), "git command failed: {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn init_git(source: &Path) -> String {
        git(source, &["init", "--quiet"]);
        git(source, &["branch", "-M", "main"]);
        commit_git(source, "fixture")
    }

    fn commit_git(source: &Path, message: &str) -> String {
        git(source, &["add", "."]);
        git(
            source,
            &[
                "-c",
                "user.name=SD07 Test",
                "-c",
                "user.email=sd07@example.invalid",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        );
        git(source, &["rev-parse", "HEAD"])
    }

    #[tokio::test]
    async fn local_invalid_update_keeps_working_version() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fixture(&source, "1.0.0");
        let installer = PluginInstaller::new(temp.path().join("user/plugins"), registry(temp.path()));

        let installed = installer
            .install(PluginInstallSource::LocalPath { path: source.clone() })
            .await
            .unwrap();
        assert_eq!(installed.version, "1.0.0");

        std::fs::write(source.join(EXTENSION_MANIFEST_FILE), b"not json").unwrap();
        assert!(
            installer
                .install(PluginInstallSource::LocalPath { path: source })
                .await
                .is_err()
        );
        let bytes = std::fs::read(installed.directory.join(EXTENSION_MANIFEST_FILE)).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(manifest["version"], "1.0.0");
    }

    #[tokio::test]
    async fn installs_from_local_git_repository() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("git-source");
        fixture(&source, "2.0.0");

        let git = |args: &[&str]| {
            let status = Command::new("git").current_dir(&source).args(args).status().unwrap();
            assert!(status.success(), "git command failed: {args:?}");
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=SD05 Test",
            "-c",
            "user.email=sd05@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);

        let installer = PluginInstaller::new(temp.path().join("user/plugins"), registry(temp.path()));
        let installed = installer
            .install(PluginInstallSource::Git {
                url: source.to_string_lossy().into_owned(),
                git_ref: None,
                subdirectory: None,
            })
            .await
            .unwrap();
        assert_eq!(installed.version, "2.0.0");
        assert!(installed.directory.join(EXTENSION_MANIFEST_FILE).is_file());
    }

    #[test]
    fn immutable_github_tree_url_resolves_repository_ref_and_subdirectory() {
        let source = normalize_git_source(
            "https://github.com/trailofbits/skills/tree/442fc9d6c89b1e937e6f7a477e7071ea75fcbea4/plugins/vulnerability-triage-brocards",
            None,
            None,
        )
        .unwrap();
        assert_eq!(source.url, "https://github.com/trailofbits/skills.git");
        assert_eq!(
            source.git_ref.as_deref(),
            Some("442fc9d6c89b1e937e6f7a477e7071ea75fcbea4")
        );
        assert_eq!(
            source.subdirectory.as_deref(),
            Some(Path::new("plugins/vulnerability-triage-brocards"))
        );

        let error = normalize_git_source(
            "https://github.com/example/repository/tree/feature/plugin/path",
            None,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
        assert!(error.to_string().contains("explicit ref and subdirectory"));

        assert!(validate_git_url("ssh://git@github.com/example/repository.git").is_ok());
        let error = validate_git_url("https://user:secret@github.com/example/repository.git").unwrap_err();
        assert!(error.to_string().contains("embedded credentials"));
        for url in [
            "https://github.com/example/repository.git?token=secret",
            "https://github.com/example/repository.git#secret",
        ] {
            let error = validate_git_url(url).unwrap_err();
            assert!(error.to_string().contains("query parameters or fragments"));
        }
    }

    #[tokio::test]
    async fn installs_only_selected_git_subdirectory_and_pins_revision() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("monorepo");
        fixture(&source.join("plugins/selected"), "3.1.4");
        fixture(&source.join("plugins/not-selected"), "9.9.9");
        let revision = init_git(&source);
        let install_root = temp.path().join("user/plugins");
        let registry = registry(temp.path());
        registry
            .initialize_with_scan_paths(vec![crate::loader::ScanPath {
                path: install_root.clone(),
                source: ExtensionSource::Local,
            }])
            .await
            .unwrap();
        let installer = PluginInstaller::new(install_root, registry.clone());

        let installed = installer
            .install(PluginInstallSource::Git {
                url: source.to_string_lossy().into_owned(),
                git_ref: Some(revision.clone()),
                subdirectory: Some(PathBuf::from("plugins/selected")),
            })
            .await
            .unwrap();

        assert_eq!(installed.version, "3.1.4");
        assert!(!installed.directory.join("plugins/not-selected").exists());
        assert_eq!(
            installed.provenance,
            PluginInstallProvenance::Git {
                url: source.to_string_lossy().into_owned(),
                git_ref: Some(revision.clone()),
                subdirectory: Some(PathBuf::from("plugins/selected")),
                resolved_revision: revision.clone(),
            }
        );
        assert_eq!(
            installer.provenance("fixture-plugin").await.unwrap(),
            Some(installed.provenance)
        );
        installer.uninstall("fixture-plugin").await.unwrap();
        assert!(registry.get_extension_by_name("fixture-plugin").await.is_none());
        assert!(!installed.directory.exists());
    }

    #[tokio::test]
    async fn git_subdirectory_update_reuses_selection_and_records_new_revision() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("monorepo");
        let plugin = source.join("plugins/selected");
        fixture(&plugin, "1.0.0");
        let first_revision = init_git(&source);
        let installer = PluginInstaller::new(temp.path().join("user/plugins"), registry(temp.path()));
        installer
            .install(PluginInstallSource::Git {
                url: source.to_string_lossy().into_owned(),
                git_ref: Some("main".into()),
                subdirectory: Some(PathBuf::from("plugins/selected")),
            })
            .await
            .unwrap();

        fixture(&plugin, "2.0.0");
        let second_revision = commit_git(&source, "update");
        assert_ne!(first_revision, second_revision);
        let updated = installer.update("fixture-plugin").await.unwrap();
        assert_eq!(updated.version, "2.0.0");
        assert_eq!(
            updated.provenance,
            PluginInstallProvenance::Git {
                url: source.to_string_lossy().into_owned(),
                git_ref: Some("main".into()),
                subdirectory: Some(PathBuf::from("plugins/selected")),
                resolved_revision: second_revision,
            }
        );
    }

    #[tokio::test]
    async fn update_rejects_manifest_rename_without_touching_either_plugin() {
        let temp = tempfile::tempdir().unwrap();
        let source_a = temp.path().join("source-a");
        fixture(&source_a, "1.0.0");
        init_git(&source_a);
        let source_b = temp.path().join("source-b");
        fixture(&source_b, "7.0.0");
        rename_fixture(&source_b, "other-plugin", "7.0.0");
        let installer = PluginInstaller::new(temp.path().join("user/plugins"), registry(temp.path()));
        let installed_a = installer
            .install(PluginInstallSource::Git {
                url: source_a.to_string_lossy().into_owned(),
                git_ref: Some("main".into()),
                subdirectory: None,
            })
            .await
            .unwrap();
        let installed_b = installer
            .install(PluginInstallSource::LocalPath { path: source_b.clone() })
            .await
            .unwrap();

        rename_fixture(&source_a, "other-plugin", "2.0.0");
        commit_git(&source_a, "rename plugin");
        let error = installer.update("fixture-plugin").await.unwrap_err();
        assert!(error.to_string().contains("expected 'fixture-plugin'"), "{error}");
        let a: serde_json::Value =
            serde_json::from_slice(&std::fs::read(installed_a.directory.join(EXTENSION_MANIFEST_FILE)).unwrap())
                .unwrap();
        let b: serde_json::Value =
            serde_json::from_slice(&std::fs::read(installed_b.directory.join(EXTENSION_MANIFEST_FILE)).unwrap())
                .unwrap();
        assert_eq!(a["name"], "fixture-plugin");
        assert_eq!(a["version"], "1.0.0");
        assert_eq!(b["name"], "other-plugin");
        assert_eq!(b["version"], "7.0.0");
    }

    #[tokio::test]
    async fn rejected_git_url_does_not_persist_credentials_or_plugin_data() {
        let temp = tempfile::tempdir().unwrap();
        let install_root = temp.path().join("user/plugins");
        let installer = PluginInstaller::new(install_root.clone(), registry(temp.path()));
        let error = installer
            .install(PluginInstallSource::Git {
                url: "https://github.com/example/repository.git?token=secret".into(),
                git_ref: Some("main".into()),
                subdirectory: Some(PathBuf::from("plugins/example")),
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("query parameters or fragments"), "{error}");
        assert_eq!(std::fs::read_dir(install_root).unwrap().count(), 0);
    }

    #[test]
    fn git_archive_output_is_bounded_before_zip_parsing() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), "1.0.0");
        init_git(temp.path());
        let mut command = Command::new("git");
        command
            .current_dir(temp.path())
            .args(["archive", "--format=zip", "HEAD"]);
        let error = run_git_archive_bounded(command, 64).unwrap_err();
        assert!(error.to_string().contains("exceeds 64 bytes"), "{error}");
    }

    #[tokio::test]
    async fn rejects_escaping_missing_and_symlinked_git_subdirectories() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("monorepo");
        fixture(&source.join("plugins/selected"), "1.0.0");
        #[cfg(unix)]
        std::os::unix::fs::symlink("../../outside", source.join("plugins/selected/escape-link")).unwrap();
        init_git(&source);
        let installer = PluginInstaller::new(temp.path().join("user/plugins"), registry(temp.path()));

        for invalid in ["../plugins/selected", "/plugins/selected", "plugins\\selected"] {
            let error = installer
                .install(PluginInstallSource::Git {
                    url: source.to_string_lossy().into_owned(),
                    git_ref: Some("main".into()),
                    subdirectory: Some(PathBuf::from(invalid)),
                })
                .await
                .unwrap_err();
            assert!(error.to_string().contains("confined relative path"), "{error}");
        }

        let missing = installer
            .install(PluginInstallSource::Git {
                url: source.to_string_lossy().into_owned(),
                git_ref: Some("main".into()),
                subdirectory: Some(PathBuf::from("plugins/missing")),
            })
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("does not exist"), "{missing}");

        #[cfg(unix)]
        {
            let symlink = installer
                .install(PluginInstallSource::Git {
                    url: source.to_string_lossy().into_owned(),
                    git_ref: Some("main".into()),
                    subdirectory: Some(PathBuf::from("plugins/selected")),
                })
                .await
                .unwrap_err();
            assert!(symlink.to_string().contains("may not contain symlinks"), "{symlink}");
        }
    }

    #[tokio::test]
    #[ignore = "requires network access to the pinned public fixture"]
    async fn installs_pinned_trailofbits_plugin_from_github_subdirectory_url() {
        let temp = tempfile::tempdir().unwrap();
        let installer = PluginInstaller::new(temp.path().join("user/plugins"), registry(temp.path()));
        let revision = "442fc9d6c89b1e937e6f7a477e7071ea75fcbea4";
        let installed = installer
            .install(PluginInstallSource::Git {
                url: format!(
                    "https://github.com/trailofbits/skills/tree/{revision}/plugins/vulnerability-triage-brocards"
                ),
                git_ref: None,
                subdirectory: None,
            })
            .await
            .unwrap();

        assert_eq!(installed.name, "vulnerability-triage-brocards");
        assert_eq!(installed.version, "0.1.3");
        assert!(
            installed
                .directory
                .join("skills/vulnerability-triage-brocards/SKILL.md")
                .is_file()
        );
        assert!(!installed.directory.join("hooks").exists());
        assert_eq!(
            installed.provenance,
            PluginInstallProvenance::Git {
                url: "https://github.com/trailofbits/skills.git".into(),
                git_ref: Some(revision.into()),
                subdirectory: Some(PathBuf::from("plugins/vulnerability-triage-brocards")),
                resolved_revision: revision.into(),
            }
        );
    }

    #[tokio::test]
    async fn rejects_local_source_that_contains_install_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("plugins");
        std::fs::create_dir_all(&target).unwrap();
        let installer = PluginInstaller::new(target, registry(temp.path()));

        let error = installer
            .install(PluginInstallSource::LocalPath {
                path: temp.path().to_path_buf(),
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("must not contain"));
    }

    #[tokio::test]
    async fn incompatible_update_keeps_working_version() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fixture(&source, "1.0.0");
        let installer = PluginInstaller::new(temp.path().join("user/plugins"), registry(temp.path()));
        let installed = installer
            .install(PluginInstallSource::LocalPath { path: source.clone() })
            .await
            .unwrap();

        let manifest_path = source.join(EXTENSION_MANIFEST_FILE);
        let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["version"] = "2.0.0".into();
        manifest["engine"] = serde_json::json!({ "aionui": ">=99.0.0" });
        std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

        assert!(
            installer
                .install(PluginInstallSource::LocalPath { path: source })
                .await
                .is_err()
        );
        let installed_manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(installed.directory.join(EXTENSION_MANIFEST_FILE)).unwrap()).unwrap();
        assert_eq!(installed_manifest["version"], "1.0.0");
    }

    #[tokio::test]
    async fn broken_mcp_update_keeps_working_version_on_disk() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fixture(&source, "1.0.0");
        add_node_mcp(&source, "console.log('working');\n");
        let installer = PluginInstaller::new(temp.path().join("user/plugins"), registry(temp.path()));
        let installed = installer
            .install(PluginInstallSource::LocalPath { path: source.clone() })
            .await
            .unwrap();

        fixture(&source, "2.0.0");
        add_node_mcp(&source, "const =\n");
        assert!(
            installer
                .install(PluginInstallSource::LocalPath { path: source })
                .await
                .is_err()
        );

        let installed_manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(installed.directory.join(EXTENSION_MANIFEST_FILE)).unwrap()).unwrap();
        assert_eq!(installed_manifest["version"], "1.0.0");
        assert_eq!(
            std::fs::read_to_string(installed.directory.join("server.mjs")).unwrap(),
            "console.log('working');\n"
        );
    }
}
