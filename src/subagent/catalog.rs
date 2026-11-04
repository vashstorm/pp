use super::Settings;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Definition {
    #[serde(skip)]
    pub model_role: Option<crate::config::ModelRole>,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking: Option<crate::config::ThinkLevel>,
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

pub(crate) fn discover(
    cwd: &Path,
    user: &Path,
    settings: &Settings,
    roles: &crate::config::ModelRoles,
) -> Result<Vec<Definition>> {
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
                let mut def: Definition = crate::yaml::parse(meta, "agent definition")?;
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
    let mut definitions = found
        .into_values()
        .filter(|d| !settings.disabled_agents.contains(&d.name))
        .collect::<Vec<_>>();
    definitions.extend(
        roles
            .iter()
            .filter(|(role, assignment)| {
                **role != crate::config::ModelRole::Plan && assignment.enabled
            })
            .map(|(role, assignment)| Definition {
                model_role: Some(*role),
                name: role.name().into(),
                description: format!("{} · {}", assignment.model, assignment.effort.as_str()),
                tools: None,
                model: None,
                thinking: None,
                spawns: None,
                blocking: false,
                output: None,
                prompt: String::new(),
                source: "/Roles".into(),
            }),
    );
    Ok(definitions)
}
