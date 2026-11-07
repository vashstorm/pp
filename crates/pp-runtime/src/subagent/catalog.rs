use anyhow::{Context, Result, ensure};
use pp_config::config::{ModelRole, TaskSettings};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking: Option<pp_core::model::ThinkLevel>,
    #[serde(default)]
    pub spawns: Option<Vec<String>>,
    #[serde(default)]
    pub blocking: bool,
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(skip)]
    pub prompt: String,
    #[serde(skip)]
    pub source: String,
}

pub(super) const BUILTIN_ROLES: [(&str, ModelRole); 5] = [
    ("fast", ModelRole::Fast),
    ("slow", ModelRole::Slow),
    ("vision", ModelRole::Vision),
    ("review", ModelRole::Review),
    ("explorer", ModelRole::Explorer),
];

pub(super) fn discover(cwd: &Path, user: &Path, settings: &TaskSettings) -> Vec<Definition> {
    let mut found = BTreeMap::new();
    let project = cwd
        .ancestors()
        .map(|p| p.join(".pp/agents"))
        .find(|p| p.is_dir());
    for dir in project.into_iter().chain([user.join("agents")]) {
        let mut files: Vec<_> = fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort();
        for path in files {
            let parsed = (|| -> Result<Definition> {
                let text = fs::read_to_string(&path)?.replace("\r\n", "\n");
                let text = text.strip_prefix("---\n").context("missing frontmatter")?;
                let (meta, body) = text
                    .split_once("\n---")
                    .context("unterminated frontmatter")?;
                let mut def: Definition = pp_config::yaml::parse(meta, "agent definition")?;
                ensure!(
                    !def.name.trim().is_empty() && !def.description.trim().is_empty(),
                    "role name and description must not be empty"
                );
                ensure!(
                    def.name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                    "invalid role name"
                );
                ensure!(
                    !ModelRole::ALL
                        .iter()
                        .any(|role| def.name.eq_ignore_ascii_case(role.name())),
                    "custom role name is reserved"
                );
                def.prompt = body.trim().to_owned();
                def.source = path.display().to_string();
                Ok(def)
            })();
            match parsed {
                Ok(def) => {
                    found.entry(def.name.clone()).or_insert(def);
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "skipping invalid subagent definition")
                }
            }
        }
    }
    found
        .into_values()
        .filter(|d| !settings.disabled_agents.contains(&d.name))
        .collect()
}
