//! Materialize the embedded builtin-skills corpus to disk.

use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

/// Gated by a `.version` file so this is a no-op on subsequent starts with
/// the same binary. When `AIONUI_BUILTIN_SKILLS_PATH` is set, skip
/// materialization — the override path is the source of truth in that mode.
pub(super) async fn materialize_builtin_skills(data_dir: &Path) -> Result<()> {
    let skip = std::env::var(aionui_extension::BUILTIN_SKILLS_ENV_VAR)
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if skip {
        return Ok(());
    }

    let corpus = aionui_extension::builtin_skills_corpus();
    let marker = aionui_extension::builtin_skills_materialize_marker(corpus, env!("CARGO_PKG_VERSION"));

    aionui_extension::materialize_if_needed(data_dir, corpus, &marker)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to materialize builtin skills: {e}"))?;

    // Global packs belong in the existing auto-inject corpus, so standalone
    // WebUI and every Core user receive the same skills as Electron. Reuse the
    // importer for validation, copy limits, symlink rejection and full references.
    for key in [
        "ELLM_SHA_SKILLS_DIR",
        "ELLM_SHA_BMAD_SKILLS_DIR",
        "ELLM_SHA_PONYTAIL_SKILLS_DIR",
    ] {
        if let Some(source) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            match materialize_global_skill_pack(data_dir, Path::new(&source)).await {
                Ok(count) => info!(pack = key, count, "Global eLLMsha skill pack materialized"),
                Err(error) => warn!(pack = key, error = %error, "Global eLLMsha skill pack unavailable"),
            }
        }
    }

    // Best-effort cleanup of directories left behind by pre-symlink
    // refactors. Failures are non-fatal — stale empty dirs are harmless.
    for stale in ["builtin-skills-view", "tmp", "agent-skills"] {
        let path = data_dir.join(stale);
        if path.exists()
            && let Err(e) = std::fs::remove_dir_all(&path)
        {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to clean up stale data dir entry",
            );
        }
    }
    Ok(())
}

async fn materialize_global_skill_pack(data_dir: &Path, source: &Path) -> Result<usize> {
    let staging = data_dir.join(".ellmsha-global-skills");
    let paths = aionui_extension::resolve_skill_paths(Path::new("."), &staging);
    let imported = aionui_extension::skill_service::import_skills(&paths, source).await?;
    if !imported.failed.is_empty() {
        anyhow::bail!("{} skills failed import validation", imported.failed.len());
    }
    let target_dir = data_dir.join("builtin-skills").join("auto-inject");
    tokio::fs::create_dir_all(&target_dir).await?;
    for name in &imported.imported {
        let target = target_dir.join(name);
        if let Ok(metadata) = tokio::fs::symlink_metadata(&target).await {
            if metadata.is_symlink() || metadata.is_file() {
                tokio::fs::remove_file(&target).await?;
            } else {
                tokio::fs::remove_dir_all(&target).await?;
            }
        }
        let staged = paths
            .user_skills_dir
            .join("users")
            .join("system_default_user")
            .join(name);
        tokio::fs::rename(staged, target).await?;
        tokio::fs::write(
            target_dir
                .join(name)
                .join(aionui_extension::skill_service::MANAGED_GLOBAL_SKILL_MARKER),
            "Managed eLLMsha global skill pack\n",
        )
        .await?;
    }
    Ok(imported.imported.len())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn global_pack_refresh_keeps_references_and_exposes_auto_inject_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let pack = temp.path().join("pack");
        let skill = pack.join("bmad-test");
        tokio::fs::create_dir_all(skill.join("reference")).await.unwrap();
        tokio::fs::write(
            skill.join("SKILL.md"),
            "---\nname: bmad-test\ndescription: Test BMAD memory workflow\n---\nUse TencentDB.",
        )
        .await
        .unwrap();
        tokio::fs::write(skill.join("reference/check.md"), "Read this reference.")
            .await
            .unwrap();
        assert_eq!(super::materialize_global_skill_pack(&data, &pack).await.unwrap(), 1);
        tokio::fs::write(skill.join("reference/check.md"), "Updated reference.")
            .await
            .unwrap();
        assert_eq!(super::materialize_global_skill_pack(&data, &pack).await.unwrap(), 1);
        assert_eq!(
            tokio::fs::read_to_string(data.join("builtin-skills/auto-inject/bmad-test/reference/check.md"))
                .await
                .unwrap(),
            "Updated reference."
        );
        let paths = aionui_extension::resolve_skill_paths(std::path::Path::new("."), &data);
        let skills = aionui_extension::list_available_skills(&paths).await.unwrap();
        let skill = skills.iter().find(|skill| skill.name == "bmad-test").unwrap();
        assert_eq!(skill.source, aionui_extension::SkillSource::Builtin);
        assert_eq!(
            skill.relative_location.as_deref(),
            Some("auto-inject/bmad-test/SKILL.md")
        );
    }

    #[test]
    fn materialize_marker_includes_builtin_skill_corpus_fingerprint() {
        let marker = aionui_extension::builtin_skills_materialize_marker(
            aionui_extension::builtin_skills_corpus(),
            env!("CARGO_PKG_VERSION"),
        );

        assert_ne!(marker, env!("CARGO_PKG_VERSION"));
        assert!(marker.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+builtin-skills.")));
    }
}
