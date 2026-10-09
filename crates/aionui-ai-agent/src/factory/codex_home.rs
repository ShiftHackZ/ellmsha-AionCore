//! A small Codex home overlay prevents v2 global memory instructions from leaking
//! into v3. Shared authentication and installed tools remain in the user's home;
//! only configuration and global instructions are scoped to this application.
use std::path::{Path, PathBuf};

use crate::error::AgentError;
use crate::session_context::{AgentSessionContext, AgentSessionKind};

pub(super) async fn isolate_legacy_memory(
    context: &mut AgentSessionContext,
    data_dir: &Path,
) -> Result<(), AgentError> {
    let AgentSessionKind::Acp(acp) = &context.kind else {
        return Ok(());
    };
    if acp.config.backend.as_deref() != Some("codex") || std::env::var_os("AIONUI_TDAI_MEMORY_CONFIG").is_none() {
        return Ok(());
    }
    let source = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| AgentError::internal("Codex home is unavailable"))?;
    let target = data_dir.join("ellmsha-codex-home");
    prepare_home(&source, &target)
        .await
        .map_err(|error| AgentError::internal(format!("Could not prepare eLLMsha Codex profile: {error}")))?;
    context.runtime_env.retain(|(name, _)| name != "CODEX_HOME");
    context
        .runtime_env
        .push(("CODEX_HOME".into(), target.to_string_lossy().into_owned()));
    Ok(())
}

async fn prepare_home(source: &Path, target: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(target).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let original = match tokio::fs::read_to_string(source.join("config.toml")).await {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let config = isolated_config(&original, source).map_err(|_| {
        // A TOML diagnostic can quote credentials from the source line.
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Codex configuration could not be parsed or serialized",
        )
    })?;
    tokio::fs::write(target.join("config.toml"), config).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(target.join("config.toml"), std::fs::Permissions::from_mode(0o600)).await?;
    }
    // Never copy credentials, histories, or plugin caches. Existing conversations
    // retain their Codex resume anchors through the shared session store.
    for name in [
        "auth.json",
        ".credentials.json",
        "skills",
        "plugins",
        "sessions",
        "archived_sessions",
        "rules",
    ] {
        let original = source.join(name);
        let link = target.join(name);
        if original.exists() && tokio::fs::symlink_metadata(&link).await.is_err() {
            link_shared(&original, &link)?;
        }
    }
    let mut instructions = String::new();
    for name in ["AGENTS.override.md", "AGENTS.md"] {
        if let Ok(text) = tokio::fs::read_to_string(source.join(name)).await
            && !text.trim().is_empty()
        {
            instructions = strip_legacy_memory_block(&text);
            break;
        }
    }
    tokio::fs::write(target.join("AGENTS.md"), instructions).await?;
    Ok(())
}

fn isolated_config(original: &str, source: &Path) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let mut config: toml::Table = toml::from_str(original)?;
    let plugins = config
        .entry("plugins")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    if let Some(plugins) = plugins.as_table_mut() {
        for (id, plugin) in plugins.iter_mut() {
            if (id == "project-memory" || id.starts_with("project-memory@"))
                && let Some(plugin) = plugin.as_table_mut()
            {
                plugin.insert("enabled".into(), toml::Value::Boolean(false));
            }
        }
    }
    if let Some(servers) = config.get_mut("mcp_servers").and_then(toml::Value::as_table_mut)
        && let Some(legacy) = servers.get_mut("ellmsha").and_then(toml::Value::as_table_mut)
    {
        legacy.insert("enabled".into(), toml::Value::Boolean(false));
    }
    config
        .entry("sqlite_home")
        .or_insert_with(|| toml::Value::String(source.to_string_lossy().into_owned()));
    Ok(toml::to_string_pretty(&config)?)
}

fn strip_legacy_memory_block(text: &str) -> String {
    const START: &str = "<!-- BEGIN PROJECT-MEMORY MANAGED BLOCK -->";
    const END: &str = "<!-- END PROJECT-MEMORY MANAGED BLOCK -->";
    let Some(start) = text.find(START) else {
        return text.to_owned();
    };
    let Some(relative_end) = text[start..].find(END) else {
        return text.to_owned();
    };
    let end = start + relative_end + END.len();
    let block = &text[start..end];
    let mut result = text[..start].to_owned();
    // These are user workflow preferences embedded in the old compiled snapshot,
    // not memory storage rules. Retain their actual guidance, without old links.
    for subject in [
        "user-git-commit-workflow-preference",
        "git-branch-naming-policy",
        "git-feature-branch-upstream-policy",
    ] {
        let heading = format!("## Global context: wiki/{subject}.md");
        if let Some(section) = block
            .split_once(&heading)
            .map(|(_, rest)| rest.split("## Global context:").next().unwrap_or(rest))
            && let Some(body) = section
                .split_once("## For future agents\n")
                .map(|(_, rest)| rest.split("\n## ").next().unwrap_or(rest))
        {
            result.push('\n');
            result.push_str(body.trim());
            result.push('\n');
        }
    }
    result.push_str(&text[end..]);
    result
}

#[cfg(unix)]
fn link_shared(source: &Path, target: &Path) -> std::io::Result<()> {
    match std::os::unix::fs::symlink(source, target) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        result => result,
    }
}

#[cfg(windows)]
fn link_shared(source: &Path, target: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, target)
    } else {
        std::os::windows::fs::symlink_file(source, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_profile_disables_only_legacy_memory() {
        let config = isolated_config("[plugins.\"project-memory@personal\"]\nenabled=true\n[plugins.other]\nenabled=true\n[mcp_servers.ellmsha]\ncommand='node'\n[mcp_servers.docs]\ncommand='docs'", Path::new("/original")).unwrap();
        let config: toml::Table = toml::from_str(&config).unwrap();
        assert_eq!(
            config["plugins"]["project-memory@personal"]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(config["plugins"]["other"]["enabled"].as_bool(), Some(true));
        assert_eq!(config["mcp_servers"]["ellmsha"]["enabled"].as_bool(), Some(false));
        assert_eq!(config["mcp_servers"]["docs"]["command"].as_str(), Some("docs"));
    }

    #[test]
    fn removes_old_memory_authority_preserving_user_rules() {
        let original = "custom before\n<!-- BEGIN PROJECT-MEMORY MANAGED BLOCK -->\nUse old memory\n## Global context: wiki/git-branch-naming-policy.md\n# title\n## For future agents\nUse feature/ branches.\n## Evidence\nold evidence\n<!-- END PROJECT-MEMORY MANAGED BLOCK -->\ncustom after";
        let filtered = strip_legacy_memory_block(original);
        assert!(filtered.contains("custom before"));
        assert!(filtered.contains("custom after"));
        assert!(filtered.contains("Use feature/ branches."));
        assert!(!filtered.contains("old memory"));
        assert!(!filtered.contains("old evidence"));
    }
}
