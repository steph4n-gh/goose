//! Native filesystem skill discovery under caller-supplied roots.

use goose_sdk_types::custom_requests::{SourceEntry, SourceType};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::warn;

/// A host-selected search root; earlier roots take precedence.
#[derive(Debug, Clone)]
pub struct SkillRoot {
    /// Directory searched recursively for `SKILL.md`. Returned entry paths identify
    /// the containing skill directory, not the Markdown file.
    /// Relative roots are resolved against the process working directory.
    pub path: PathBuf,
    /// Scope metadata copied to each entry, not inferred from its location.
    pub global: bool,
    /// Writability metadata copied to each entry. Discovery does not check
    /// permissions or enforce whether a host may modify the skill.
    pub writable: bool,
    /// Retain lexical skill-directory and supporting-file paths (e.g. plugin links)
    /// instead of canonicalizing the skill directory. Relative roots then produce
    /// relative result paths; supply absolute roots when absolute paths are needed.
    /// This does not change symlink traversal or confine reads to the root.
    pub preserve_path: bool,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    metadata: HashMap<String, Value>,
}

fn parse_frontmatter<T: for<'de> Deserialize<'de>>(
    content: &str,
) -> Result<Option<(T, String)>, serde_yaml::Error> {
    let parts: Vec<&str> = content.split("---").collect();
    if parts.len() < 3 {
        return Ok(None);
    }

    let yaml_content = parts[1].trim();
    let metadata: T = serde_yaml::from_str(yaml_content)?;

    let body = parts[2..].join("---").trim().to_string();
    Ok(Some((metadata, body)))
}

enum ParsedSkill {
    Skill(SourceEntry),
    InvalidFrontmatter(String),
    Skip,
}

fn parse_skill_content(
    content: &str,
    skill_dir: &Path,
    skill_file: &Path,
    global: bool,
    writable: bool,
) -> ParsedSkill {
    let (metadata, body): (Frontmatter, String) = match parse_frontmatter(content) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => return ParsedSkill::Skip,
        Err(error) => {
            warn!(
                "Failed to parse skill frontmatter in {}: {}",
                skill_file.display(),
                error
            );
            return ParsedSkill::InvalidFrontmatter(error.to_string());
        }
    };

    let name = match metadata.name.filter(|n| !n.is_empty()) {
        Some(n) => n,
        None => {
            warn!(
                "Skill at '{}' is missing a required 'name' in frontmatter, skipping",
                skill_dir.display()
            );
            return ParsedSkill::Skip;
        }
    };

    if name.contains('/') {
        warn!("Skill name '{}' contains '/', skipping", name);
        return ParsedSkill::Skip;
    }

    ParsedSkill::Skill(SourceEntry {
        source_type: SourceType::Skill,
        name,
        description: metadata.description,
        content: body,
        path: skill_dir.to_string_lossy().into_owned(),
        global,
        writable,
        supporting_files: Vec::new(),
        properties: metadata.metadata,
    })
}

struct ScannedSkills {
    skills: Vec<SourceEntry>,
    skipped: Vec<SkippedSkill>,
}

fn should_skip_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git") | Some(".hg") | Some(".svn")
    )
}

fn walk_files_recursively<F, G>(
    dir: &Path,
    visited_dirs: &mut HashSet<PathBuf>,
    should_descend: &mut G,
    visit_file: &mut F,
) where
    F: FnMut(&Path),
    G: FnMut(&Path) -> bool,
{
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(_) => return,
    };

    if !visited_dirs.insert(canonical_dir) {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if should_descend(&path) {
                walk_files_recursively(&path, visited_dirs, should_descend, visit_file);
            }
        } else if path.is_file() {
            visit_file(&path);
        }
    }
}

fn scan_skills_from_dir(
    dir: &Path,
    global: bool,
    writable: bool,
    preserve_path: bool,
    seen: &mut HashSet<String>,
) -> ScannedSkills {
    let mut skill_files = Vec::new();
    let mut visited_dirs = HashSet::new();

    walk_files_recursively(
        dir,
        &mut visited_dirs,
        &mut |path| !should_skip_dir(path),
        &mut |path| {
            if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
                skill_files.push(path.to_path_buf());
            }
        },
    );

    let mut skills = Vec::new();
    let mut skipped = Vec::new();
    for skill_file in skill_files {
        let Some(skill_dir) = skill_file.parent() else {
            continue;
        };
        let registered_skill_dir = if preserve_path {
            skill_dir.to_path_buf()
        } else {
            let Ok(canonical_dir) = skill_dir.canonicalize() else {
                continue;
            };
            canonical_dir
        };
        let content = match std::fs::read_to_string(&skill_file) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read skill file {}: {}", skill_file.display(), e);
                continue;
            }
        };

        match parse_skill_content(
            &content,
            &registered_skill_dir,
            &skill_file,
            global,
            writable,
        ) {
            ParsedSkill::InvalidFrontmatter(error) => {
                skipped.push(SkippedSkill {
                    path: skill_file.clone(),
                    error,
                });
            }
            ParsedSkill::Skip => {}
            ParsedSkill::Skill(mut source) => {
                if !seen.contains(&source.name) {
                    let mut files = Vec::new();
                    let mut visited_support_dirs = HashSet::new();
                    walk_files_recursively(
                        skill_dir,
                        &mut visited_support_dirs,
                        &mut |path| !should_skip_dir(path) && !path.join("SKILL.md").is_file(),
                        &mut |path| {
                            if path.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
                                if let Ok(relative) = path.strip_prefix(skill_dir) {
                                    files.push(
                                        registered_skill_dir
                                            .join(relative)
                                            .to_string_lossy()
                                            .into_owned(),
                                    );
                                }
                            }
                        },
                    );
                    source.supporting_files = files;

                    seen.insert(source.name.clone());
                    skills.push(source);
                }
            }
        }
    }
    ScannedSkills { skills, skipped }
}

/// A `SKILL.md` whose YAML frontmatter failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSkill {
    pub path: PathBuf,
    pub error: String,
}

/// Skills found under the supplied roots, plus frontmatter that failed to parse.
#[derive(Debug)]
pub struct SkillDiscovery {
    pub skills: Vec<SourceEntry>,
    pub skipped: Vec<SkippedSkill>,
}

/// Recursively discovers skills from explicit roots, without consulting host configuration.
///
/// Roots are visited in input order. The first successfully parsed entry with a
/// given name wins; names are compared exactly and case-sensitively. Traversal
/// within each root is unsorted, so result order, duplicate-name winners within a
/// root, and supporting-file order are unspecified.
///
/// Discovery is best-effort: missing or unreadable directories and invalid or
/// unreadable skills are skipped. Some failures emit tracing warnings; no errors
/// are returned. YAML frontmatter that fails to parse is also reported by
/// [`discover_skills_with_skipped`]. An empty result does not distinguish no
/// skills from a failed scan.
///
/// Traversal follows file and directory symlinks, including targets outside a
/// root. Canonical directory identities prevent cycles. `preserve_path` only
/// affects returned path spelling; discovery is not a confined file-open API.
/// The caller must enforce access policy when opening returned paths.
pub fn discover_skills(roots: impl IntoIterator<Item = SkillRoot>) -> Vec<SourceEntry> {
    discover_skills_with_skipped(roots).skills
}

/// Same scan as [`discover_skills`], also returning `SKILL.md` files whose YAML
/// frontmatter failed to parse. Other skips, such as a missing name or an
/// unreadable file, are still warning-only.
pub fn discover_skills_with_skipped(roots: impl IntoIterator<Item = SkillRoot>) -> SkillDiscovery {
    let mut skills = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();

    for dir in roots {
        let scanned = scan_skills_from_dir(
            &dir.path,
            dir.global,
            dir.writable,
            dir.preserve_path,
            &mut seen,
        );
        skills.extend(scanned.skills);
        skipped.extend(scanned.skipped);
    }

    SkillDiscovery { skills, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    struct MessageCapture {
        messages: Arc<Mutex<Vec<String>>>,
    }

    struct MessageVisitor {
        message: String,
    }

    impl Visit for MessageVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{value:?}");
            }
        }
    }

    impl Subscriber for MessageCapture {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= &tracing::Level::WARN
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = MessageVisitor {
                message: String::new(),
            };
            event.record(&mut visitor);
            if !visitor.message.is_empty() {
                self.messages.lock().unwrap().push(visitor.message);
            }
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "goose-skill-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn invalid_yaml_frontmatter_warns_with_the_skill_path() {
        let root = TempDir::new();
        let broken_dir = root.path().join("broken");
        std::fs::create_dir(&broken_dir).unwrap();
        let broken_file = broken_dir.join("SKILL.md");
        std::fs::write(
            &broken_file,
            "---\nname: broken\ndescription: This breaks YAML: colon space here\n---\n# test\n",
        )
        .unwrap();
        let ok_dir = root.path().join("ok");
        std::fs::create_dir(&ok_dir).unwrap();
        std::fs::write(
            ok_dir.join("SKILL.md"),
            "---\nname: ok\ndescription: Fine\n---\nbody\n",
        )
        .unwrap();

        let messages = Arc::new(Mutex::new(Vec::new()));
        let discovered = tracing::subscriber::with_default(
            MessageCapture {
                messages: Arc::clone(&messages),
            },
            || {
                discover_skills_with_skipped([SkillRoot {
                    path: root.path().to_path_buf(),
                    global: false,
                    writable: true,
                    preserve_path: true,
                }])
            },
        );

        assert_eq!(discovered.skills.len(), 1);
        assert_eq!(discovered.skills[0].name, "ok");
        assert_eq!(discovered.skipped.len(), 1);
        assert_eq!(discovered.skipped[0].path, broken_file);
        assert!(
            discovered.skipped[0]
                .error
                .contains("mapping values are not allowed"),
            "{}",
            discovered.skipped[0].error
        );
        let logged = messages.lock().unwrap().join("\n");
        assert!(
            logged.contains(&format!(
                "Failed to parse skill frontmatter in {}",
                broken_file.display()
            )),
            "{logged}"
        );
        assert!(
            logged.contains("mapping values are not allowed"),
            "{logged}"
        );
        assert!(!logged.contains(&format!("{}", ok_dir.join("SKILL.md").display())));
    }
}
