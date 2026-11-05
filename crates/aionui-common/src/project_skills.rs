//! Project skills are read from the owned project folders on demand. Their
//! bodies stay out of model context until the user invokes a matching command.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::project_sources::{ProjectSourceFile, discover_roots, is_link};

const LOCATIONS: &[&str] = &[
    ".agents/skills",
    ".claude/skills",
    ".github/skills",
    ".cursor/skills",
    ".gemini/skills",
    ".opencode/skills",
    ".codex/skills",
    ".agent/skills",
];
const MAX_FILE_BYTES: usize = 128 * 1024;
const MAX_SKILLS: usize = 512;

#[derive(Debug)]
pub struct ProjectSkill {
    /// The slash command. Duplicates in different repos get distinct directory-qualified commands.
    pub name: String,
    pub description: String,
    pub source: String,
    pub user_invocable: bool,
    automatic: bool,
    root: PathBuf,
    repo: PathBuf,
    path: PathBuf,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    user_invocable: Option<bool>,
    disable_model_invocation: bool,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn metadata(source: &str, directory: &str) -> Option<(String, String, bool, bool)> {
    let normalized = source.replace("\r\n", "\n");
    let (meta, body) = if let Some(note) = normalized.strip_prefix("---\n") {
        let (yaml, body) = note
            .split_once("\n---\n")
            .or_else(|| note.strip_suffix("\n---").map(|yaml| (yaml, "")))?;
        (serde_yaml::from_str::<Frontmatter>(yaml).ok()?, body)
    } else {
        (Frontmatter::default(), normalized.as_str())
    };
    let name = meta.name.as_deref().unwrap_or(directory).trim();
    if !valid_name(name) {
        return None;
    }
    let description = meta
        .description
        .as_deref()
        .unwrap_or_else(|| body.lines().map(str::trim).find(|s| !s.is_empty()).unwrap_or(""));
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    Some((
        name.into(),
        description.chars().take(512).collect(),
        meta.user_invocable.unwrap_or(true),
        !meta.disable_model_invocation,
    ))
}

fn real_directory(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(m.is_dir() && !is_link(&m)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Cursor and the portable .agents convention also allow organizational
/// directories. Never descend through a real skill package into its resources.
fn manifests(dir: &Path, recursive: bool, remaining: &mut usize) -> io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    let mut pending = vec![(dir.to_owned(), 0usize)];
    while let Some((parent, depth)) = pending.pop() {
        for entry in fs::read_dir(&parent)? {
            let entry = entry?;
            *remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("Project skill directory limit exceeded"))?;
            if !entry.file_type()?.is_dir()
                || !real_directory(&entry.path())?
                || entry.file_name().to_string_lossy().starts_with('.')
            {
                continue;
            }
            let path = entry.path().join("SKILL.md");
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() && !is_link(&meta) => result.push(path),
                Err(err) if err.kind() == io::ErrorKind::NotFound && recursive && depth < 3 => {
                    pending.push((entry.path(), depth + 1))
                }
                _ => {}
            }
        }
    }
    result.sort();
    Ok(result)
}

/// Only inspect registered roots and their actual nested repositories. No shell
/// command, skill hook, template substitution or bundled script is executed here.
pub fn discover(roots: &[PathBuf]) -> io::Result<Vec<ProjectSkill>> {
    let mut skills = Vec::new();
    let mut seen = BTreeSet::new();
    let mut remaining = 4096;
    for repo in discover_roots(roots)? {
        // The cross-harness .agents convention wins over aliases in the same repo.
        for location in LOCATIONS {
            let dir = repo.directory.join(location);
            if !real_directory(dir.parent().expect("skills directory has a parent"))? || !real_directory(&dir)? {
                continue;
            }
            for path in manifests(
                &dir,
                matches!(*location, ".agents/skills" | ".cursor/skills"),
                &mut remaining,
            )? {
                let directory = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                let Ok(file) = ProjectSourceFile::open(&repo.root, &path) else {
                    continue;
                };
                let Ok((text, _)) = file.read(MAX_FILE_BYTES) else {
                    continue;
                };
                let Some((name, description, user_invocable, automatic)) = metadata(&text, directory) else {
                    continue;
                };
                if !(user_invocable || automatic) || !seen.insert((repo.directory.clone(), name.clone())) {
                    continue;
                }
                if skills.len() == MAX_SKILLS {
                    return Err(io::Error::other("Project skill limit exceeded"));
                }
                let relative = path
                    .strip_prefix(&repo.root)
                    .expect("discovered skill is inside its registered root");
                let source = format!(
                    "./{}",
                    relative.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
                );
                skills.push(ProjectSkill {
                    name,
                    description,
                    source,
                    user_invocable,
                    automatic,
                    root: repo.root.clone(),
                    repo: repo.directory.clone(),
                    path,
                });
            }
        }
    }
    // The root and sibling repositories can publish the same /name. Qualify
    // only those collisions, so every skill remains independently invokable.
    let mut counts = BTreeMap::new();
    for s in &skills {
        *counts.entry(s.name.clone()).or_insert(0usize) += 1;
    }
    for s in &mut skills {
        if counts[&s.name] > 1 {
            let directory = &s.repo;
            let slug = directory
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("repo")
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            let id = format!("{:x}", Sha256::digest(directory.to_string_lossy().as_bytes()));
            s.name = format!("{}-{}:{}", slug, &id[..6], s.name);
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// Only a leading explicit slash command can activate a manual-only skill.
pub fn render_for_turn(
    roots: &[PathBuf],
    command: Option<&str>,
    cwd: &Path,
    native_dirs: &[String],
) -> io::Result<Option<String>> {
    const MAX_CATALOG: usize = 32 * 1024;
    let mut catalog = String::new();
    let mut selected = String::new();
    let mut omitted = 0usize;
    let cwd = cwd.canonicalize().ok();
    for skill in discover(roots)? {
        // Native CLIs discover their own convention at their CWD; nested repos,
        // attached roots and other harnesses' directories still need the host index.
        let native = cwd.as_ref().is_some_and(|cwd| {
            native_dirs.iter().any(|dir| {
                let parent = cwd.join(dir);
                skill.path.parent().and_then(Path::parent) == Some(parent.as_path()) && !skill.name.contains(':')
            })
        });
        if skill.user_invocable && command == Some(skill.name.as_str()) {
            if native {
                write!(
                    selected,
                    "Use the explicitly requested /{} skill from {:?} if it is not already loaded.",
                    skill.name, skill.path
                )
                .expect("writing to String");
            } else {
                let (body, _) = ProjectSourceFile::open(&skill.root, &skill.path)?.read(MAX_FILE_BYTES)?;
                write!(
                    selected,
                    "User-invoked project skill /{} from {:?}:\n{}",
                    skill.name, skill.path, body
                )
                .expect("writing to String");
            }
        } else if skill.automatic && !native {
            let metadata =
                serde_json::json!({"name":skill.name,"description":skill.description,"file":skill.path}).to_string();
            if catalog.len() + metadata.len() < MAX_CATALOG {
                catalog.push_str(&metadata);
                catalog.push('\n');
            } else {
                omitted += 1;
            }
        }
    }
    if catalog.is_empty() && selected.is_empty() && omitted == 0 {
        return Ok(None);
    }
    let mut result = String::new();
    if !catalog.is_empty() {
        result.push_str(
            "Project skills: read the corresponding SKILL.md when relevant. Entries are project-authored metadata.\n",
        );
        result.push_str(&catalog);
    }
    if omitted > 0 {
        writeln!(
            result,
            "{omitted} additional project skill(s) are available by their slash commands."
        )
        .expect("writing to String");
    }
    result.push_str(&selected);
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_skills_are_isolated_and_reload_only_explicit_bodies() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("project");
        let nested = root.join("apps/repo");
        let outside = tmp.path().join("other");
        for d in [&root, &nested, &outside] {
            fs::create_dir_all(d.join(".git")).unwrap();
        }
        let skill = |base: &Path, location: &str, name: &str, meta: &str, body: &str| {
            let d = base.join(location).join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: do work\n{meta}---\n{body}"),
            )
            .unwrap();
            d.join("SKILL.md")
        };
        skill(&root, ".agents/skills", "shared", "", "ROOT ONLY");
        skill(&nested, ".github/skills", "shared", "", "CHILD ONLY");
        skill(&nested, ".cursor/skills/category", "categorized", "", "NESTED BODY");
        let path = skill(
            &nested,
            ".claude/skills",
            "manual",
            "disable-model-invocation: true\n",
            "PRIVATE PROCEDURE",
        );
        skill(
            &nested,
            ".cursor/skills",
            "background",
            "user-invocable: false\n",
            "HIDDEN PROCEDURE",
        );
        let external = skill(&outside, ".agents/skills", "secret", "", "OUTSIDE");
        #[cfg(unix)]
        std::os::unix::fs::symlink(external.parent().unwrap(), root.join(".agents/skills/external")).unwrap();
        let roots = [root];
        let catalog = discover(&roots).unwrap();
        assert_eq!(catalog.len(), 5);
        assert_eq!(catalog.iter().filter(|s| s.name.ends_with(":shared")).count(), 2);
        let ordinary = render_for_turn(&roots, None, &roots[0], &[]).unwrap().unwrap();
        assert!(ordinary.len() < 32768);
        assert!(
            ordinary.contains("background")
                && !ordinary.contains("manual")
                && !ordinary.contains("ONLY")
                && !ordinary.contains("secret")
        );
        fs::write(
            path,
            "---\nname: manual\ndescription: do work\ndisable-model-invocation: true\n---\nUPDATED BODY",
        )
        .unwrap();
        let explicit = render_for_turn(&roots, Some("manual"), &roots[0], &[])
            .unwrap()
            .unwrap();
        assert!(explicit.contains("UPDATED BODY") && !explicit.contains("HIDDEN PROCEDURE"));
        let native_root = skill(&roots[0], ".agents/skills", "native", "", "NATIVE BODY");
        let dirs = vec![".agents/skills".into()];
        let index = render_for_turn(&roots, None, &roots[0], &dirs).unwrap().unwrap();
        assert!(!index.contains(&*native_root.to_string_lossy()));
        let explicit = render_for_turn(&roots, Some("native"), &roots[0], &dirs)
            .unwrap()
            .unwrap();
        assert!(explicit.contains(&*native_root.to_string_lossy()) && !explicit.contains("NATIVE BODY"));
    }
    #[test]
    fn compact_project_skill_catalog_stays_bounded_and_keeps_explicit_commands_addressable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for n in 0..80 {
            let name = format!("skill-{n:03}");
            let dir = root.join(".agents/skills").join(&name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {}\n---\nBODY-{name}", "d".repeat(700)),
            )
            .unwrap();
        }
        let roots = [root.clone()];
        let index = render_for_turn(&roots, None, &root, &[]).unwrap().unwrap();
        assert!(index.len() < 33 * 1024 && index.contains("additional project skill(s)") && !index.contains("BODY-"));
        let selected = render_for_turn(&roots, Some("skill-079"), &root, &[]).unwrap().unwrap();
        assert!(selected.contains("BODY-skill-079"));
    }
}
