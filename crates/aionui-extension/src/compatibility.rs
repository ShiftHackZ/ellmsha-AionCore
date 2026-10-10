//! Import adapters for portable Agent Plugins, Codex, and Claude packages.
//!
//! The adapter produces the existing native manifest model. Runtime loading,
//! revision leasing, enablement, watcher behavior, and MCP approval remain
//! owned by the extension registry.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::ExtensionError;
use crate::types::{
    ExtContributes, ExtMcpServer, ExtPermissions, ExtSkill, ExtensionManifest, FilesystemScope, NetworkPermission,
    PluginCompatibility, PluginCompatibilityNotice, PluginPackageFormat,
};

const PORTABLE_SCHEMA_MARKER: &str = "agent-plugins.org/schemas/";

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

/// Detect and normalize a supported third-party plugin package.
pub(crate) fn load_compatible_manifest(directory: &Path) -> Result<ExtensionManifest, ExtensionError> {
    let portable_path = directory.join("plugin.json");
    if portable_path.is_file() {
        let root = read_json(&portable_path)?;
        if root
            .get("$schema")
            .and_then(Value::as_str)
            .is_some_and(|schema| schema.contains(PORTABLE_SCHEMA_MARKER))
        {
            return normalize_package(directory, root, PluginPackageFormat::AgentPlugin);
        }
    }

    let codex_path = directory.join(".codex-plugin/plugin.json");
    if codex_path.is_file() {
        return normalize_package(directory, read_json(&codex_path)?, PluginPackageFormat::Codex);
    }

    let claude_path = directory.join(".claude-plugin/plugin.json");
    if claude_path.is_file() {
        return normalize_package(directory, read_json(&claude_path)?, PluginPackageFormat::Claude);
    }

    Err(ExtensionError::ManifestValidation(format!(
        "no supported manifest found in {}; expected aion-extension.json, portable plugin.json, .codex-plugin/plugin.json, or .claude-plugin/plugin.json",
        directory.display()
    )))
}

pub(crate) fn has_compatible_manifest(directory: &Path) -> bool {
    directory.join("plugin.json").is_file()
        || directory.join(".codex-plugin/plugin.json").is_file()
        || directory.join(".claude-plugin/plugin.json").is_file()
}

fn normalize_package(
    directory: &Path,
    manifest: Value,
    format: PluginPackageFormat,
) -> Result<ExtensionManifest, ExtensionError> {
    let root = manifest
        .as_object()
        .ok_or_else(|| ExtensionError::ManifestValidation("compatible plugin manifest must be a JSON object".into()))?;
    let name = required_string(root, "name")?;
    let mut warnings = Vec::new();
    let version = normalize_version(root.get("version").and_then(Value::as_str), &mut warnings);
    let openai = root
        .get("extensions")
        .and_then(Value::as_object)
        .and_then(|extensions| extensions.get("com.openai"))
        .and_then(Value::as_object);
    let interface = openai
        .and_then(|value| value.get("interface"))
        .or_else(|| root.get("interface"))
        .and_then(Value::as_object);

    let display_name =
        string_field(root, "displayName").or_else(|| interface.and_then(|value| string_field(value, "displayName")));
    let description = string_field(root, "description")
        .or_else(|| interface.and_then(|value| string_field(value, "shortDescription")));
    let author = root.get("author").and_then(author_name);
    let homepage =
        string_field(root, "homepage").or_else(|| interface.and_then(|value| string_field(value, "websiteURL")));
    let icon = string_field(root, "icon")
        .or_else(|| interface.and_then(|value| string_field(value, "logo")))
        .or_else(|| interface.and_then(|value| string_field(value, "composerIcon")));

    let mut supported = Vec::new();
    let mut unsupported = unsupported_components(root, openai, format, directory);
    let skill_roots = skill_roots(root, directory, format)?;
    let skills = discover_skills(directory, &skill_roots)?;
    if !skills.is_empty() {
        supported.push("skills".to_owned());
    }

    let mcp_servers = discover_mcp_servers(directory, root, format, &mut unsupported)?;
    if !mcp_servers.is_empty() {
        supported.push("mcp_servers".to_owned());
    }
    let mut seen = HashSet::new();
    unsupported.retain(|notice| seen.insert(notice.component.clone()));
    if skills.is_empty() && mcp_servers.is_empty() {
        return Err(ExtensionError::ManifestValidation(format!(
            "plugin '{name}' has no supported skills or MCP servers; unsupported components: {}",
            unsupported
                .iter()
                .map(|notice| notice.component.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let permissions = inferred_permissions(&mcp_servers);
    Ok(ExtensionManifest {
        name,
        version,
        display_name,
        description,
        author,
        license: string_field(root, "license"),
        homepage,
        icon,
        engine: None,
        api_version: None,
        dependencies: HashMap::new(),
        entry_point: None,
        permissions,
        contributes: Some(ExtContributes {
            skills,
            mcp_servers,
            ..Default::default()
        }),
        lifecycle: None,
        i18n: None,
        compatibility: Some(PluginCompatibility {
            format,
            supported_components: supported,
            unsupported_components: unsupported,
            warnings,
        }),
    })
}

fn read_json(path: &Path) -> Result<Value, ExtensionError> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ExtensionError::ManifestValidation(format!("{} is invalid JSON: {error}", path.display())))
}

fn required_string(root: &Map<String, Value>, key: &str) -> Result<String, ExtensionError> {
    string_field(root, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ExtensionError::ManifestValidation(format!("compatible plugin manifest requires a non-empty '{key}'"))
        })
}

fn string_field(root: &Map<String, Value>, key: &str) -> Option<String> {
    root.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn author_name(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.get("name").and_then(Value::as_str).map(str::to_owned))
}

fn normalize_version(version: Option<&str>, warnings: &mut Vec<PluginCompatibilityNotice>) -> String {
    match version.filter(|version| semver::Version::parse(version).is_ok()) {
        Some(version) => version.to_owned(),
        None => {
            warnings.push(PluginCompatibilityNotice {
                component: "version".into(),
                message: "The source package omitted a semantic version or used a non-semantic version; eLLMsha normalized it to 0.0.0.".into(),
            });
            "0.0.0".into()
        }
    }
}

fn unsupported_components(
    root: &Map<String, Value>,
    openai: Option<&Map<String, Value>>,
    format: PluginPackageFormat,
    directory: &Path,
) -> Vec<PluginCompatibilityNotice> {
    let mut notices = Vec::new();
    let keys = [
        ("commands", "Commands are not loaded by eLLMsha."),
        ("agents", "Claude/Codex agent definitions are not loaded by eLLMsha."),
        (
            "hooks",
            "Lifecycle hooks require host-specific trust and are not executed by eLLMsha.",
        ),
        ("lspServers", "Language servers are not loaded by the plugin runtime."),
        ("outputStyles", "Output styles are not supported."),
        ("workflows", "Host workflows are not supported."),
        ("channels", "Claude channels are not supported."),
        ("userConfig", "Host-managed userConfig prompts are not supported."),
        ("settings", "Host-specific plugin settings are not applied."),
        ("types", "Plugin state type declarations are not loaded."),
    ];
    for (key, message) in keys {
        if root.contains_key(key) {
            notices.push(notice(key, message));
        }
    }
    if root.contains_key("experimental") {
        notices.push(notice(
            "experimental",
            "Experimental themes, monitors, and eval components are not loaded.",
        ));
    }
    if let Some(openai) = openai {
        for (key, message) in [
            ("apps", "OpenAI app UI mappings are not supported."),
            ("hooks", "Codex lifecycle hooks are not executed or silently trusted."),
            (
                "onboardingSkill",
                "Automatic onboarding invocation is not supported; the skill remains discoverable.",
            ),
        ] {
            if openai.contains_key(key) {
                notices.push(notice(key, message));
            }
        }
    }
    let default_components: &[(&str, &str, &str)] = match format {
        PluginPackageFormat::Claude => &[
            ("commands", "commands", "Claude commands are not loaded by eLLMsha."),
            (
                "agents",
                "agents",
                "Claude agent definitions are not loaded by eLLMsha.",
            ),
            (
                "hooks",
                "hooks/hooks.json",
                "Claude lifecycle hooks are not executed by eLLMsha.",
            ),
            (
                "lspServers",
                ".lsp.json",
                "Claude language servers are not loaded by eLLMsha.",
            ),
            (
                "outputStyles",
                "output-styles",
                "Claude output styles are not supported.",
            ),
            ("workflows", "workflows", "Claude workflows are not supported."),
            (
                "themes",
                "themes",
                "Claude themes are not loaded by the plugin runtime.",
            ),
            (
                "monitors",
                "monitors/monitors.json",
                "Claude monitors are not loaded by eLLMsha.",
            ),
            ("bin", "bin", "Claude plugin executables are not added to PATH."),
            ("settings", "settings.json", "Claude plugin settings are not applied."),
        ],
        PluginPackageFormat::Codex => &[(
            "hooks",
            "hooks/hooks.json",
            "Codex lifecycle hooks are not executed by eLLMsha.",
        )],
        PluginPackageFormat::AgentPlugin | PluginPackageFormat::Native => &[],
    };
    for (component, path, message) in default_components {
        if directory.join(path).exists() {
            notices.push(notice(component, message));
        }
    }
    let mut seen = HashSet::new();
    notices.retain(|notice| seen.insert(notice.component.clone()));
    notices
}

fn notice(component: &str, message: &str) -> PluginCompatibilityNotice {
    PluginCompatibilityNotice {
        component: component.into(),
        message: message.into(),
    }
}

fn skill_roots(
    root: &Map<String, Value>,
    directory: &Path,
    format: PluginPackageFormat,
) -> Result<Vec<PathBuf>, ExtensionError> {
    let mut roots = Vec::new();
    if format == PluginPackageFormat::Claude && directory.join("SKILL.md").is_file() {
        roots.push(PathBuf::new());
    }
    if directory.join("skills").is_dir() {
        roots.push(PathBuf::from("skills"));
    }
    if let Some(value) = root.get("skills") {
        match value {
            Value::String(path) => roots.push(validate_component_path(path)?),
            Value::Array(paths) => {
                for path in paths {
                    let path = path
                        .as_str()
                        .ok_or_else(|| ExtensionError::ManifestValidation("skills entries must be paths".into()))?;
                    roots.push(validate_component_path(path)?);
                }
            }
            _ => {
                return Err(ExtensionError::ManifestValidation(
                    "skills must be a path or array of paths".into(),
                ));
            }
        }
    }
    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert(root.clone()));
    Ok(roots)
}

fn discover_skills(directory: &Path, roots: &[PathBuf]) -> Result<Vec<ExtSkill>, ExtensionError> {
    let mut skills = Vec::new();
    let mut names = HashSet::new();
    let canonical_directory = std::fs::canonicalize(directory)?;
    for relative_root in roots {
        let root = confined_existing_path(directory, relative_root)?;
        if !root.is_dir() {
            return Err(ExtensionError::ManifestValidation(format!(
                "skill path '{}' is not a directory",
                relative_root.display()
            )));
        }
        let direct = root.join("SKILL.md");
        if direct.is_file() {
            push_skill(&canonical_directory, &direct, &mut names, &mut skills)?;
            continue;
        }
        let mut entries = std::fs::read_dir(&root)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let skill_file = entry.path().join("SKILL.md");
            if entry.file_type()?.is_dir() && skill_file.is_file() {
                push_skill(&canonical_directory, &skill_file, &mut names, &mut skills)?;
            }
        }
    }
    Ok(skills)
}

fn push_skill(
    directory: &Path,
    skill_file: &Path,
    names: &mut HashSet<String>,
    skills: &mut Vec<ExtSkill>,
) -> Result<(), ExtensionError> {
    crate::skill_service::validate_skill_file(skill_file)?;
    let source = std::fs::read_to_string(skill_file)?;
    let frontmatter = parse_skill_frontmatter(&source).ok_or_else(|| {
        ExtensionError::ManifestValidation(format!(
            "skill {} requires YAML frontmatter with name and description",
            skill_file.display()
        ))
    })?;
    if !names.insert(frontmatter.name.clone()) {
        return Err(ExtensionError::ManifestValidation(format!(
            "duplicate skill name '{}'",
            frontmatter.name
        )));
    }
    let relative = skill_file
        .strip_prefix(directory)
        .map_err(|_| ExtensionError::PathTraversal(skill_file.to_string_lossy().into_owned()))?;
    skills.push(ExtSkill {
        name: frontmatter.name,
        description: frontmatter.description,
        path: Some(relative.to_string_lossy().into_owned()),
    });
    Ok(())
}

fn parse_skill_frontmatter(source: &str) -> Option<SkillFrontmatter> {
    let rest = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---").or_else(|| rest.find("\r\n---"))?;
    serde_yaml::from_str(&rest[..end]).ok()
}

fn discover_mcp_servers(
    directory: &Path,
    root: &Map<String, Value>,
    format: PluginPackageFormat,
    unsupported: &mut Vec<PluginCompatibilityNotice>,
) -> Result<Vec<ExtMcpServer>, ExtensionError> {
    let mut sources = Vec::new();
    let default_name = if format == PluginPackageFormat::AgentPlugin {
        "mcp.json"
    } else {
        ".mcp.json"
    };
    let default_path = directory.join(default_name);
    if default_path.is_file() {
        sources.push(read_json(&default_path)?);
    }
    if let Some(value) = root.get("mcpServers") {
        collect_mcp_sources(directory, value, &mut sources, unsupported)?;
    }

    let mut by_name = Map::new();
    for source in sources {
        let servers = source
            .get("mcpServers")
            .and_then(Value::as_object)
            .or_else(|| source.as_object())
            .ok_or_else(|| {
                ExtensionError::ManifestValidation("MCP configuration must contain an mcpServers object".into())
            })?;
        for (name, config) in servers {
            by_name.insert(name.clone(), config.clone());
        }
    }

    let mut result = Vec::new();
    for (name, mut config) in by_name {
        let config_obj = config
            .as_object_mut()
            .ok_or_else(|| ExtensionError::ManifestValidation(format!("MCP server '{name}' must be an object")))?;
        if config_obj
            .get("extensions")
            .and_then(Value::as_object)
            .and_then(|extensions| extensions.get("com.openai"))
            .and_then(Value::as_object)
            .is_some_and(|openai| openai.contains_key("auth"))
        {
            unsupported.push(notice(
                "mcp_auth",
                &format!("MCP server '{name}' declares OpenAI authentication, which eLLMsha does not import."),
            ));
            continue;
        }
        normalize_mcp_config(config_obj, directory)?;
        result.push(ExtMcpServer {
            id: name.clone(),
            name,
            description: None,
            auto_approve_in_unattended: false,
            config: Value::Object(config_obj.clone()),
        });
    }
    Ok(result)
}

fn collect_mcp_sources(
    directory: &Path,
    value: &Value,
    sources: &mut Vec<Value>,
    unsupported: &mut Vec<PluginCompatibilityNotice>,
) -> Result<(), ExtensionError> {
    match value {
        Value::String(path) if path.ends_with(".json") => {
            let path = validate_component_path(path)?;
            sources.push(read_json(&confined_existing_path(directory, &path)?)?);
        }
        Value::String(path) => unsupported.push(notice(
            "mcp_bundle",
            &format!("MCP bundle or URL '{path}' is not supported; use a JSON MCP configuration."),
        )),
        Value::Object(_) => sources.push(value.clone()),
        Value::Array(values) => {
            for value in values {
                collect_mcp_sources(directory, value, sources, unsupported)?;
            }
        }
        _ => {
            return Err(ExtensionError::ManifestValidation(
                "mcpServers must be a path, object, or array".into(),
            ));
        }
    }
    Ok(())
}

fn normalize_mcp_config(config: &mut Map<String, Value>, directory: &Path) -> Result<(), ExtensionError> {
    reject_literal_credentials(config)?;
    if let Some(Value::String(kind)) = config.get_mut("type") {
        if kind == "streamable-http" {
            *kind = "streamable_http".into();
        }
    } else if config.contains_key("command") {
        config.insert("type".into(), Value::String("stdio".into()));
    } else if config.contains_key("url") {
        config.insert("type".into(), Value::String("streamable_http".into()));
    }
    let kind = config
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !matches!(kind.as_str(), "stdio" | "http" | "sse" | "streamable_http") {
        return Err(ExtensionError::ManifestValidation(format!(
            "unsupported MCP transport '{kind}'"
        )));
    }
    replace_plugin_root(Value::Object(config.clone()), directory).and_then(|value| {
        *config = value.as_object().cloned().unwrap_or_default();
        if kind == "stdio" && config.get("command").and_then(Value::as_str).is_none() {
            return Err(ExtensionError::ManifestValidation(
                "stdio MCP server requires command".into(),
            ));
        }
        if kind != "stdio" && config.get("url").and_then(Value::as_str).is_none() {
            return Err(ExtensionError::ManifestValidation(
                "remote MCP server requires url".into(),
            ));
        }
        Ok(())
    })
}

fn reject_literal_credentials(config: &Map<String, Value>) -> Result<(), ExtensionError> {
    for section in ["env", "headers"] {
        let Some(values) = config.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (key, value) in values {
            let Some(value) = value.as_str() else {
                continue;
            };
            let upper = key.to_ascii_uppercase();
            let sensitive = upper.contains("TOKEN")
                || upper.contains("SECRET")
                || upper.contains("PASSWORD")
                || upper.contains("API_KEY")
                || upper == "AUTHORIZATION"
                || upper == "X-API-KEY";
            if sensitive && !value.is_empty() && !value.contains("${") {
                return Err(ExtensionError::ManifestValidation(format!(
                    "MCP server contains a literal credential in {section}.{key}; use an environment variable"
                )));
            }
        }
    }
    Ok(())
}

fn replace_plugin_root(value: Value, directory: &Path) -> Result<Value, ExtensionError> {
    match value {
        Value::String(value) => {
            validate_plugin_root_reference(&value)?;
            let root = directory.to_string_lossy();
            Ok(Value::String(
                value
                    .replace("${CLAUDE_PLUGIN_ROOT}", &root)
                    .replace("${CODEX_PLUGIN_ROOT}", &root)
                    .replace("${PLUGIN_ROOT}", &root),
            ))
        }
        Value::Array(values) => values
            .into_iter()
            .map(|value| replace_plugin_root(value, directory))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| Ok((key, replace_plugin_root(value, directory)?)))
            .collect::<Result<Map<_, _>, ExtensionError>>()
            .map(Value::Object),
        other => Ok(other),
    }
}

fn validate_plugin_root_reference(value: &str) -> Result<(), ExtensionError> {
    if value.contains("${user_config.") {
        return Err(ExtensionError::ManifestValidation(
            "MCP configuration references Claude userConfig, which eLLMsha does not support".into(),
        ));
    }
    if value.contains(":-") && value.contains("${") {
        return Err(ExtensionError::ManifestValidation(
            "MCP environment default expressions are not supported; declare a plain environment variable".into(),
        ));
    }
    for marker in ["${CLAUDE_PLUGIN_DATA}", "${CLAUDE_PROJECT_DIR}"] {
        if value.contains(marker) {
            return Err(ExtensionError::ManifestValidation(format!(
                "MCP configuration references unsupported Claude variable {marker}"
            )));
        }
    }
    for marker in ["${CLAUDE_PLUGIN_ROOT}", "${CODEX_PLUGIN_ROOT}", "${PLUGIN_ROOT}"] {
        if let Some((prefix, suffix)) = value.split_once(marker) {
            if !prefix.is_empty() {
                return Err(ExtensionError::ManifestValidation(format!(
                    "plugin root variable in '{value}' must start the path"
                )));
            }
            let suffix = suffix.trim_start_matches(['/', '\\']);
            if !suffix.is_empty() {
                validate_component_path(suffix)?;
            }
        }
    }
    Ok(())
}

fn inferred_permissions(servers: &[ExtMcpServer]) -> Option<ExtPermissions> {
    if servers.is_empty() {
        return None;
    }
    let has_stdio = servers
        .iter()
        .any(|server| server.config.get("type").and_then(Value::as_str) == Some("stdio"));
    Some(ExtPermissions {
        shell: has_stdio.then_some(true),
        filesystem: has_stdio.then_some(FilesystemScope::Full),
        // A local MCP subprocess can open sockets just as a remote transport
        // can. Compatibility packages have no enforceable domain declaration,
        // so the catalog uses the conservative unrestricted disclosure.
        network: Some(NetworkPermission::Unrestricted(true)),
        ..Default::default()
    })
}

fn validate_component_path(path: &str) -> Result<PathBuf, ExtensionError> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
    {
        return Err(ExtensionError::PathTraversal(path.to_string_lossy().into_owned()));
    }
    let normalized = path
        .components()
        .filter_map(|part| match part {
            Component::CurDir => None,
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<PathBuf>();
    if normalized.as_os_str().is_empty() {
        return Err(ExtensionError::ManifestValidation(
            "component path must not be empty".into(),
        ));
    }
    Ok(normalized)
}

fn confined_existing_path(directory: &Path, relative: &Path) -> Result<PathBuf, ExtensionError> {
    let base = std::fs::canonicalize(directory)?;
    let candidate = std::fs::canonicalize(directory.join(relative)).map_err(|error| {
        ExtensionError::ManifestValidation(format!(
            "component path '{}' is unavailable: {error}",
            relative.display()
        ))
    })?;
    if !candidate.starts_with(&base) {
        return Err(ExtensionError::PathTraversal(relative.to_string_lossy().into_owned()));
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_component_paths() {
        let error = validate_component_path("../outside").unwrap_err();
        assert!(matches!(error, ExtensionError::PathTraversal(_)));
    }

    #[test]
    fn normalizes_non_semantic_version_with_warning() {
        let mut warnings = Vec::new();
        assert_eq!(normalize_version(Some("next"), &mut warnings), "0.0.0");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn rejects_plugin_root_escape() {
        let error = validate_plugin_root_reference("${CLAUDE_PLUGIN_ROOT}/../outside.js").unwrap_err();
        assert!(matches!(error, ExtensionError::PathTraversal(_)));
        validate_plugin_root_reference("${PLUGIN_ROOT}/server.mjs").unwrap();
    }

    #[test]
    fn rejects_unsupported_user_config_and_literal_credentials() {
        let error = validate_plugin_root_reference("${user_config.api_key}").unwrap_err();
        assert!(matches!(error, ExtensionError::ManifestValidation(_)));
        let error = validate_plugin_root_reference("${CLAUDE_PLUGIN_DATA}/state.json").unwrap_err();
        assert!(matches!(error, ExtensionError::ManifestValidation(_)));

        let config = serde_json::json!({
            "env": { "API_KEY": "plain-text-secret" },
        });
        let error = reject_literal_credentials(config.as_object().unwrap()).unwrap_err();
        assert!(matches!(error, ExtensionError::ManifestValidation(_)));

        let referenced = serde_json::json!({
            "headers": { "Authorization": "Bearer ${PLUGIN_TOKEN}" },
        });
        reject_literal_credentials(referenced.as_object().unwrap()).unwrap();
    }

    #[test]
    fn imports_installed_ponytail_and_mobile_package_contracts() {
        let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
        let ponytail = load_compatible_manifest(&repository_root.join("ponytail")).unwrap();
        assert_eq!(ponytail.name, "ponytail");
        assert_eq!(
            ponytail.compatibility.as_ref().unwrap().format,
            PluginPackageFormat::Codex
        );
        assert!(
            ponytail
                .contributes
                .as_ref()
                .unwrap()
                .skills
                .iter()
                .any(|skill| skill.name == "ponytail")
        );
        assert!(
            ponytail
                .compatibility
                .as_ref()
                .unwrap()
                .unsupported_components
                .iter()
                .any(|notice| notice.component == "hooks")
        );

        let mobile = load_compatible_manifest(&repository_root.join("mobile-mcp/plugin")).unwrap();
        assert_eq!(mobile.name, "mobile-mcp");
        assert_eq!(
            mobile.compatibility.as_ref().unwrap().format,
            PluginPackageFormat::Claude
        );
        let server = &mobile.contributes.as_ref().unwrap().mcp_servers[0];
        assert_eq!(server.config["type"], "stdio");
        assert!(!server.auto_approve_in_unattended);
    }
}
