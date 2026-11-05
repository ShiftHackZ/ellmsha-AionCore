//! Project rules belong to their repository files, never to the memory index.
mod document;
#[cfg(test)]
mod tests;

use crate::{AgentError, types::MemoryRecallScope};
use aionui_api_types::{ProjectFileRuleMutation, ProjectRuleFile, ProjectRuleFiles};
use aionui_common::project_sources::{ProjectSourceFile, ProjectSourceRoot, SourceEditError, discover_roots, is_link};
use document::Document;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_FILE: usize = 128 * 1024;
const MAX_TOTAL: usize = 512 * 1024;
const MAX_FILES: usize = 128;
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn digest(raw: &[u8]) -> String {
    format!("{:x}", Sha256::digest(raw))
}
fn failed() -> AgentError {
    AgentError::internal("Cannot safely read project rule files")
}
fn limit() -> AgentError {
    AgentError::bad_request("Project rule discovery limit exceeded")
}
fn scope_roots(scope: &MemoryRecallScope) -> Vec<PathBuf> {
    if scope.isolated_test || scope.temporary_project || scope.project_id.is_none() {
        return vec![];
    }
    scope.roots.iter().map(PathBuf::from).collect()
}

struct Source {
    authority: ProjectSourceRoot,
    absolute: PathBuf,
    bytes: usize,
    instructions: String,
    view: ProjectRuleFile,
}

/// Only files in the documented instruction locations are candidates, never
/// arbitrary Markdown, skill bodies, imports, repository settings or BMAD output.
fn collect_directory(dir: &Path, suffix: &str, out: &mut Vec<PathBuf>, visited: &mut usize) -> Result<(), AgentError> {
    let m = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(failed()),
    };
    if is_link(&m) || !m.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|_| failed())? {
        *visited += 1;
        if *visited > 4096 {
            return Err(limit());
        }
        let entry = entry.map_err(|_| failed())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || name.contains(['<', '>', '\\']) || name.chars().any(char::is_control) {
            continue;
        }
        let p = entry.path();
        let m = fs::symlink_metadata(&p).map_err(|_| failed())?;
        if is_link(&m) {
            continue;
        }
        if m.is_dir() {
            collect_directory(&p, suffix, out, visited)?;
        } else if m.is_file() && name.ends_with(suffix) {
            out.push(p);
        }
        if out.len() > MAX_FILES {
            return Err(limit());
        }
    }
    Ok(())
}

fn metadata_if_present(path: &Path) -> Result<Option<fs::Metadata>, AgentError> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(m)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(_) => Err(failed()),
    }
}

fn candidates(repo: &Path) -> Result<Vec<PathBuf>, AgentError> {
    let mut paths = Vec::new();
    for name in [
        "AGENTS.override.md",
        "AGENTS.md",
        "CLAUDE.md",
        "CLAUDE.local.md",
        ".claude/CLAUDE.md",
        "GEMINI.md",
        ".github/copilot-instructions.md",
        ".claude/AGENTS.md",
    ] {
        let p = repo.join(name);
        let parent = p.parent().ok_or_else(failed)?;
        if parent != repo && !metadata_if_present(parent)?.is_some_and(|m| m.is_dir() && !is_link(&m)) {
            continue;
        }
        if metadata_if_present(&p)?.is_some_and(|m| m.is_file() && !is_link(&m)) {
            paths.push(p);
        }
    }
    let mut visited = 0;
    for (folder, suffix) in [
        (".claude/rules", ".md"),
        (".github/instructions", ".instructions.md"),
        (".cursor/rules", ".mdc"),
    ] {
        // Never enumerate through a linked hidden root, including directory junctions.
        let top = repo.join(folder.split('/').next().unwrap());
        if metadata_if_present(&top)?.is_some_and(|m| m.is_dir() && !is_link(&m)) {
            collect_directory(&repo.join(folder), suffix, &mut paths, &mut visited)?;
        }
    }
    Ok(paths)
}

fn applicability(path: &Path, doc: &Document<'_>) -> Option<String> {
    let portable = path.to_string_lossy().replace('\\', "/");
    let cursor = portable.starts_with(".cursor/rules/");
    let copilot = portable.starts_with(".github/instructions/");
    let claude = portable.starts_with(".claude/rules/");
    if !cursor && !copilot && !claude {
        return None;
    }
    let yaml = doc
        .frontmatter
        .and_then(|s| serde_yaml::from_str::<serde_yaml::Value>(s).ok());
    if doc.frontmatter.is_some() && yaml.is_none() {
        return Some("Invalid frontmatter; consult the file before using this rule".into());
    }
    if cursor && yaml.as_ref().and_then(|obj| obj["alwaysApply"].as_bool()) == Some(true) {
        return None;
    }
    let key = if cursor {
        "globs"
    } else if copilot {
        "applyTo"
    } else {
        "paths"
    };
    if let Some(value) = yaml.as_ref().and_then(|y| y.get(key)) {
        let paths = match value {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Sequence(values) => {
                values.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")
            }
            _ => String::new(),
        };
        if !paths.is_empty() {
            return Some(format!("Files matching: {paths}"));
        }
        return Some("Unrecognized path selector; consult the file before using this rule".into());
    }
    if cursor {
        return Some(
            yaml.as_ref()
                .and_then(|v| v["description"].as_str())
                .filter(|s| !s.is_empty())
                .map(|s| format!("When relevant: {s}"))
                .unwrap_or_else(|| "Only when explicitly requested".into()),
        );
    }
    copilot.then(|| "Path-specific instructions; consult the file’s applyTo selector".into())
}

/// Disambiguate equally named attachments with their shortest different suffix.
fn root_labels(roots: impl Iterator<Item = PathBuf>) -> BTreeMap<PathBuf, String> {
    let roots: BTreeSet<_> = roots.collect();
    roots
        .iter()
        .map(|root| {
            let mut tail = PathBuf::new();
            for component in root.components().rev() {
                tail = PathBuf::from(component.as_os_str()).join(tail);
                if roots.iter().filter(|other| other.ends_with(&tail)).count() == 1 {
                    break;
                }
            }
            let label = tail.to_string_lossy().trim_end_matches(['/', '\\']).replace('\\', "/");
            (root.clone(), label)
        })
        .collect()
}
fn collect(roots: &[PathBuf], cwd: &Path) -> Result<Vec<Source>, AgentError> {
    let mut result = Vec::new();
    let mut total = 0;
    let discovered = discover_roots(roots).map_err(|_| failed())?;
    let mut directories: BTreeMap<PathBuf, ProjectSourceRoot> = discovered
        .into_iter()
        .map(|root| (root.directory.clone(), root))
        .collect();
    if let Ok(active) = cwd.canonicalize() {
        let nearest = directories
            .values()
            .filter(|r| active.starts_with(&r.directory))
            .max_by_key(|r| r.directory.components().count())
            .cloned();
        if let Some(parent) = nearest {
            let mut current = parent.directory.clone();
            for component in active
                .strip_prefix(&parent.directory)
                .map_err(|_| failed())?
                .components()
            {
                let name = component.as_os_str().to_string_lossy();
                if aionui_common::project_sources::ignored_directory(&name) {
                    break;
                }
                current.push(component);
                directories.entry(current.clone()).or_insert_with(|| ProjectSourceRoot {
                    directory: current.clone(),
                    root: parent.root.clone(),
                });
            }
        }
    }
    let labels = root_labels(directories.values().map(|r| r.root.clone()));
    for authority in directories.into_values() {
        let mut paths = candidates(&authority.directory)?;
        paths.sort();
        let override_path = authority.directory.join("AGENTS.override.md");
        let mut sections = Vec::new();
        for p in paths {
            let file = match ProjectSourceFile::open(&authority.root, &p) {
                Ok(file) => file,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(_) => return Err(failed()),
            };
            let (raw, mode) = file.read(MAX_FILE).map_err(|_| failed())?;
            total += raw.len();
            if total > MAX_TOTAL || result.len() + sections.len() >= MAX_FILES {
                return Err(limit());
            }
            let relative = p
                .strip_prefix(&authority.root)
                .map_err(|_| failed())?
                .to_str()
                .ok_or_else(failed)?
                .replace('\\', "/");
            let repo_path = p.strip_prefix(&authority.directory).map_err(|_| failed())?;
            let doc = Document::parse(&raw, p.file_name().and_then(|s| s.to_str()).ok_or_else(failed)?)?;
            let condition = applicability(repo_path, &doc);
            let rules = doc.rules();
            let instructions = doc.instructions();
            let id = digest(p.as_os_str().as_encoded_bytes());
            let view = ProjectRuleFile {
                id,
                path: format!("./{relative}"),
                root_label: labels.get(&authority.root).cloned().ok_or_else(failed)?,
                revision: digest(raw.as_bytes()),
                writable: !mode.readonly(),
                condition,
                rules,
            };
            sections.push(Source {
                authority: authority.clone(),
                absolute: p,
                bytes: raw.len(),
                instructions,
                view,
            });
        }
        let overridden = sections
            .iter()
            .any(|s| s.absolute == override_path && !s.view.rules.is_empty());
        for mut s in sections {
            if overridden && s.absolute == authority.directory.join("AGENTS.md") {
                s.view.condition = Some("Superseded by AGENTS.override.md in this directory".into());
            }
            result.push(s);
        }
    }
    Ok(result)
}

fn editor_workspace(scope: &MemoryRecallScope) -> PathBuf {
    crate::capability::tencent_memory::live_rule_workspace(scope)
        .or_else(|| (!scope.rule_workspace.is_empty()).then(|| scope.rule_workspace.clone()))
        .unwrap_or_else(|| scope.workspace.clone())
        .into()
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
) -> Result<T, AgentError> {
    tokio::task::spawn_blocking(f).await.map_err(|_| failed())?
}
pub async fn list(scope: &MemoryRecallScope) -> Result<ProjectRuleFiles, AgentError> {
    let roots = scope_roots(scope);
    let project_id = scope
        .project_id
        .clone()
        .filter(|_| !scope.isolated_test && !scope.temporary_project);
    let cwd = editor_workspace(scope);
    blocking(move || {
        Ok(ProjectRuleFiles {
            project_id,
            files: {
                let active = cwd.canonicalize().ok();
                collect(&roots, &cwd)?
                    .into_iter()
                    .map(|s| {
                        let mut view = s.view;
                        if !active
                            .as_ref()
                            .is_some_and(|cwd| cwd.starts_with(&s.authority.directory))
                        {
                            let domain = s
                                .authority
                                .directory
                                .strip_prefix(&s.authority.root)
                                .unwrap_or(Path::new(""));
                            let scope = format!("Applies under {} / ./{}", view.root_label, domain.display());
                            view.condition =
                                Some(view.condition.map_or(scope.clone(), |rule| format!("{scope}. {rule}")));
                        }
                        view
                    })
                    .collect()
            },
        })
    })
    .await
}
pub async fn mutate(scope: &MemoryRecallScope, change: ProjectFileRuleMutation) -> Result<(), AgentError> {
    let roots = scope_roots(scope);
    let cwd = editor_workspace(scope);
    blocking(move || {
        let _guard = WRITE_LOCK.lock().map_err(|_| failed())?;
        let (file_id, revision) = match &change {
            ProjectFileRuleMutation::Add { file_id, revision, .. }
            | ProjectFileRuleMutation::Edit { file_id, revision, .. }
            | ProjectFileRuleMutation::Delete { file_id, revision, .. } => (file_id, revision),
        };
        let sources = collect(&roots, &cwd)?;
        let total: usize = sources.iter().map(|s| s.bytes).sum();
        let source = sources
            .into_iter()
            .find(|s| &s.view.id == file_id)
            .ok_or_else(|| AgentError::conflict("This rule file is no longer in the project"))?;
        if source.view.revision != *revision {
            return Err(AgentError::conflict(
                "The project rule file changed; refresh before saving",
            ));
        }
        if !source.view.writable {
            return Err(AgentError::forbidden("Project rule file is read-only"));
        }
        let file = ProjectSourceFile::open(&source.authority.root, &source.absolute).map_err(|_| failed())?;
        let (raw, permissions) = file.read(MAX_FILE).map_err(|_| failed())?;
        if digest(raw.as_bytes()) != *revision {
            return Err(AgentError::conflict(
                "The project rule file changed; refresh before saving",
            ));
        }
        let edited = Document::parse(&raw, source.absolute.file_name().unwrap().to_str().ok_or_else(failed)?)?
            .update(&change)?;
        if edited.len() > MAX_FILE || total - source.bytes + edited.len() > MAX_TOTAL {
            return Err(limit());
        }
        file.replace(revision, &edited, permissions, MAX_FILE)
            .map_err(|err| match err {
                SourceEditError::Conflict => {
                    AgentError::conflict("The project rule file changed; refresh before saving")
                }
                SourceEditError::Io(_) => AgentError::forbidden("Cannot save the project rule file"),
            })
    })
    .await
}

/// Only the current repository's unconditional guidance is inline. Other
/// repositories and path-scoped sources are discoverable by reference. No
/// historical compiled memory blocks are included in either representation.
pub(crate) async fn prompt(scope: &MemoryRecallScope, cwd: &str) -> Result<Vec<serde_json::Value>, AgentError> {
    let roots = scope_roots(scope);
    let cwd = PathBuf::from(cwd);
    blocking(move || render_file_rules(&roots, &cwd)).await
}

fn render_file_rules(roots: &[PathBuf], cwd: &Path) -> Result<Vec<serde_json::Value>, AgentError> {
    let work = cwd.canonicalize().ok();
    let mut output = Vec::new();
    let mut bodies = BTreeSet::new();
    let sources = collect(roots, cwd)?;
    for source in sources {
        if source.view.rules.is_empty() {
            continue;
        }
        let label = display_source(&source);
        let domain = source
            .authority
            .directory
            .strip_prefix(&source.authority.root)
            .map_err(|_| failed())?;
        let in_workspace = work
            .as_ref()
            .is_some_and(|p| p.starts_with(&source.authority.directory));
        let text = if let Some(condition) = source.view.condition {
            format!(
                "Within {} / ./{}: {condition}. Read the corresponding file only when applicable.",
                source.view.root_label,
                domain.display()
            )
        } else if !in_workspace {
            format!(
                "Only for work in {} / ./{}: read this file before working there.",
                source.view.root_label,
                domain.display()
            )
        } else {
            if !bodies.insert(digest(source.instructions.as_bytes())) {
                continue;
            }
            source.instructions
        };
        let row =
            serde_json::json!({"scope":"project file","path":label,"revision":source.view.revision,"content":text});
        output.push(row);
    }
    Ok(output)
}

fn display_source(source: &Source) -> String {
    format!("{} / {}", source.view.root_label, source.view.path)
        .replace('<', "‹")
        .replace('>', "›")
        .replace(char::is_control, " ")
}
