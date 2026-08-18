use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use thiserror::Error;

const MAX_SKILL_BYTES: u64 = 128 * 1024;
const MAX_DISCOVERY_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: Option<String>,
    pub path: PathBuf,
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("cannot read skill {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("skill file is larger than {MAX_SKILL_BYTES} bytes: {0}")]
    TooLarge(PathBuf),
    #[error("selected skill is not indexed: {0}")]
    NotIndexed(String),
}

#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    skills: HashMap<String, SkillMetadata>,
}

impl SkillIndex {
    pub fn discover(repository: &Path, user_locations: &[PathBuf]) -> Self {
        let mut roots = vec![repository.to_path_buf()];
        roots.extend_from_slice(user_locations);
        let mut skills = HashMap::new();

        for root in roots {
            let mut queue = VecDeque::from([(root, 0_usize)]);
            while let Some((directory, depth)) = queue.pop_front() {
                if depth > MAX_DISCOVERY_DEPTH {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&directory) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let ignored = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| matches!(name, ".git" | "target" | "node_modules"));
                        if !ignored {
                            queue.push_back((path, depth + 1));
                        }
                    } else if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")
                        && let Ok(metadata) = parse_metadata(&path)
                    {
                        skills.entry(metadata.name.clone()).or_insert(metadata);
                    }
                }
            }
        }
        Self { skills }
    }

    pub fn all(&self) -> Vec<SkillMetadata> {
        let mut skills: Vec<_> = self.skills.values().cloned().collect();
        skills.sort_by(|left, right| left.name.cmp(&right.name));
        skills
    }

    pub fn load_prompt_bundle(&self, selected: &[String]) -> Result<String, SkillError> {
        let mut bundle = String::new();
        for name in selected {
            let skill = self
                .skills
                .get(name)
                .ok_or_else(|| SkillError::NotIndexed(name.clone()))?;
            let metadata = fs::metadata(&skill.path).map_err(|source| SkillError::Read {
                path: skill.path.clone(),
                source,
            })?;
            if metadata.len() > MAX_SKILL_BYTES {
                return Err(SkillError::TooLarge(skill.path.clone()));
            }
            let instructions =
                fs::read_to_string(&skill.path).map_err(|source| SkillError::Read {
                    path: skill.path.clone(),
                    source,
                })?;
            if !bundle.is_empty() {
                bundle.push_str("\n\n");
            }
            bundle.push_str("<skill name=\"");
            bundle.push_str(&skill.name);
            bundle.push_str("\">\n");
            bundle.push_str(&instructions);
            bundle.push_str("\n</skill>");
        }
        Ok(bundle)
    }
}

fn parse_metadata(path: &Path) -> Result<SkillMetadata, SkillError> {
    let metadata = fs::metadata(path).map_err(|source| SkillError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_SKILL_BYTES {
        return Err(SkillError::TooLarge(path.to_path_buf()));
    }
    let contents = fs::read_to_string(path).map_err(|source| SkillError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let frontmatter = contents
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("---"))
        .map(|(frontmatter, _)| frontmatter)
        .unwrap_or_default();
    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        match key.trim() {
            "name" => name = Some(value.to_owned()),
            "description" => description = Some(value.to_owned()),
            _ => {}
        }
    }
    let fallback = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("unnamed-skill")
        .to_owned();
    Ok(SkillMetadata {
        name: name.unwrap_or(fallback),
        description,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_frontmatter_and_builds_prompt_bundle() {
        let root = tempdir().unwrap();
        let skill_dir = root.path().join(".agents/skills/testing");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: testing\ndescription: Test the change\n---\n# Instructions\nRun tests.",
        )
        .unwrap();

        let index = SkillIndex::discover(root.path(), &[]);
        assert_eq!(index.all()[0].name, "testing");
        let bundle = index.load_prompt_bundle(&["testing".into()]).unwrap();
        assert!(bundle.contains("Run tests."));
    }
}
