//! Parse Markdown ranges, not a Markdown serialization. Edits splice a single
//! range and retain frontmatter, comments, fences, line endings and managed data.
use crate::AgentError;
use aionui_api_types::{ProjectFileRule, ProjectFileRuleMutation};
use std::ops::Range;

const BEGIN: &str = "<!-- BEGIN PROJECT-MEMORY MANAGED BLOCK -->";
const END: &str = "<!-- END PROJECT-MEMORY MANAGED BLOCK -->";

#[derive(Debug)]
struct Part {
    range: Range<usize>,
    title: Option<Range<usize>>,
    text: Range<usize>,
}
pub(super) struct Document<'a> {
    pub frontmatter: Option<&'a str>,
    raw: &'a str,
    name: &'a str,
    parts: Vec<Part>,
}
impl<'a> Document<'a> {
    pub fn parse(raw: &'a str, name: &'a str) -> Result<Self, AgentError> {
        let body = raw.strip_prefix('\u{feff}').unwrap_or(raw);
        let mut pos = raw.len() - body.len();
        let lines = |s: &'a str| s.split_inclusive('\n');
        let mut frontmatter = None;
        let mut header = lines(body);
        if header.next().is_some_and(|s| s.trim_end_matches(['\r', '\n']) == "---") {
            let mut size = body.split_inclusive('\n').next().unwrap().len();
            let start = size;
            for line in header {
                if matches!(line.trim_end_matches(['\r', '\n']), "---" | "...") {
                    frontmatter = Some(&body[start..size]);
                    pos += size + line.len();
                    break;
                }
                size += line.len();
            }
            if frontmatter.is_none() {
                return Err(AgentError::bad_request(
                    "Unterminated frontmatter in a project rule file",
                ));
            }
        }
        let mut parts = Vec::new();
        let (mut start, mut heading) = (pos, None);
        let (mut managed, mut fence) = (false, None::<(u8, usize)>);
        let mut flush = |start: usize, end: usize, heading: Option<(Range<usize>, usize)>| {
            let (title, bstart) = heading.map_or((None, start), |(h, bs)| (Some(h), bs));
            let content = &raw[bstart..end];
            let trimmed = content.trim_matches(['\r', '\n']);
            if trimmed.trim().is_empty() && title.is_none() {
                return;
            }
            let text_start = bstart + content.len() - content.trim_start_matches(['\r', '\n']).len();
            parts.push(Part {
                range: start..end,
                title,
                text: text_start..text_start + trimmed.len(),
            });
        };
        for line in lines(&raw[pos..]) {
            let clean = line.trim_end_matches(['\r', '\n']);
            if fence.is_none() && clean.trim() == BEGIN && !managed {
                flush(start, pos, heading.take());
                managed = true;
            } else if managed {
                if clean.trim() == END {
                    managed = false;
                    start = pos + line.len();
                }
            } else if let Some((ch, n)) = fence {
                let tail = clean.trim_start_matches(' ');
                if clean.len() - tail.len() <= 3
                    && tail.trim_end().bytes().all(|b| b == ch)
                    && tail.trim_end().len() >= n
                {
                    fence = None;
                }
            } else {
                let stripped = clean.trim_start_matches(' ');
                let indent = clean.len() - stripped.len();
                if indent <= 3 && matches!(stripped.as_bytes().first(), Some(b'`' | b'~')) {
                    let ch = stripped.as_bytes()[0];
                    let n = stripped.bytes().take_while(|b| *b == ch).count();
                    if n >= 3 {
                        fence = Some((ch, n));
                    }
                } else if indent <= 3 {
                    let level = stripped.bytes().take_while(|c| *c == b'#').count();
                    if (1..=6).contains(&level) && stripped.as_bytes().get(level) == Some(&b' ') {
                        flush(start, pos, heading.take());
                        start = pos;
                        let text = stripped[level..].trim();
                        let tail = &stripped[level..];
                        let tpos = pos + indent + level + tail.len() - tail.trim_start().len();
                        heading = Some((tpos..tpos + text.len(), pos + line.len()));
                    }
                }
            }
            pos += line.len();
        }
        if managed {
            return Err(AgentError::bad_request(
                "Unterminated managed memory block in a project rule file",
            ));
        }
        flush(start, pos, heading.take());
        Ok(Self {
            frontmatter,
            raw,
            name,
            parts,
        })
    }

    pub fn instructions(&self) -> String {
        self.parts
            .iter()
            .map(|part| self.raw[part.range.clone()].trim().replace("\r\n", "\n"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn rules(&self) -> Vec<ProjectFileRule> {
        self.parts
            .iter()
            .enumerate()
            .map(|(i, p)| ProjectFileRule {
                id: i.to_string(),
                title: p
                    .title
                    .clone()
                    .map(|r| self.raw[r].to_owned())
                    .unwrap_or_else(|| self.name.to_owned()),
                content: self.raw[p.text.clone()].to_owned(),
            })
            .collect()
    }
    pub fn update(&self, change: &ProjectFileRuleMutation) -> Result<String, AgentError> {
        let nl = if self.raw.contains("\r\n") { "\r\n" } else { "\n" };
        let mut out = self.raw.to_owned();
        let fields = |title: &str, body: &str| -> Result<(String, String), AgentError> {
            if title.trim().is_empty()
                || title.chars().count() > 160
                || title.chars().any(char::is_control)
                || body.trim().is_empty()
                || body.chars().count() > 20_000
                || body.contains('\0')
                || [BEGIN, END]
                    .iter()
                    .any(|marker| title.contains(marker) || body.contains(marker))
            {
                return Err(AgentError::bad_request("Invalid project rule title or instructions"));
            }
            Ok((
                title.trim().into(),
                body.trim_end_matches(['\r', '\n'])
                    .replace("\r\n", "\n")
                    .replace('\n', nl),
            ))
        };
        let get = |id: &str| {
            id.parse::<usize>()
                .ok()
                .and_then(|n| self.parts.get(n))
                .ok_or_else(|| AgentError::conflict("Rule section has changed"))
        };
        match change {
            ProjectFileRuleMutation::Add { title, content, .. } => {
                let (title, body) = fields(title, content)?;
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push_str(nl);
                }
                if !out.is_empty() && !out.ends_with(&format!("{nl}{nl}")) {
                    out.push_str(nl);
                }
                out.push_str(&format!("## {title}{nl}{nl}{body}{nl}"));
            }
            ProjectFileRuleMutation::Edit {
                rule_id,
                title,
                content,
                ..
            } => {
                let p = get(rule_id)?;
                let (title, body) = fields(title, content)?;
                out.replace_range(p.text.clone(), &body);
                if let Some(r) = &p.title {
                    out.replace_range(r.clone(), &title);
                } else if title != self.name {
                    out.insert_str(p.range.start, &format!("## {title}{nl}{nl}"));
                }
            }
            ProjectFileRuleMutation::Delete { rule_id, .. } => {
                out.replace_range(get(rule_id)?.range.clone(), "");
            }
        }
        Ok(out)
    }
}
