use anyhow::{Context, Result, ensure};
use pp_config::config::{ModelRole, TaskSettings};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path, sync::LazyLock};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub readonly: bool,
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

impl Definition {
    fn parse(markdown: &str) -> Result<Self> {
        let markdown = markdown.replace("\r\n", "\n");
        let text = markdown
            .strip_prefix("---\n")
            .context("missing frontmatter")?;
        let (meta, body) = text
            .split_once("\n---\n")
            .or_else(|| text.strip_suffix("\n---").map(|meta| (meta, "")))
            .context("unterminated frontmatter")?;
        let mut definition: Self = pp_config::yaml::parse(meta, "agent definition")?;
        ensure!(
            !definition.name.is_empty()
                && definition
                    .name
                    .bytes()
                    .all(|b| { b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-') }),
            "invalid role name"
        );
        ensure!(
            !definition.description.trim().is_empty(),
            "role description must not be empty"
        );
        definition.prompt = body.trim().to_owned();
        Ok(definition)
    }
}

static BUILTINS: LazyLock<BTreeMap<ModelRole, Definition>> = LazyLock::new(|| {
    ModelRole::ALL
        .into_iter()
        .map(|role| {
            let markdown = match role {
                ModelRole::Default => include_str!("../prompts/system.md"),
                ModelRole::Plan => include_str!("../prompts/roles/plan.md"),
                ModelRole::Fast => include_str!("../prompts/roles/fast.md"),
                ModelRole::Slow => include_str!("../prompts/roles/slow.md"),
                ModelRole::Vision => include_str!("../prompts/roles/vision.md"),
                ModelRole::Review => include_str!("../prompts/roles/review.md"),
                ModelRole::Explorer => include_str!("../prompts/roles/explorer.md"),
            };
            let definition = Definition::parse(markdown).expect("valid bundled role Markdown");
            assert_eq!(definition.name, role.name().to_ascii_lowercase());
            (role, definition)
        })
        .collect()
});

pub(crate) fn builtin(role: ModelRole) -> &'static Definition {
    &BUILTINS[&role]
}

pub(crate) fn workflow_instructions(workflow: &pp_core::plan::WorkflowState) -> String {
    workflow.instructions(&builtin(ModelRole::Plan).prompt)
}

pub(super) fn task_roles() -> impl Iterator<Item = (ModelRole, &'static Definition)> {
    BUILTINS.iter().filter_map(|(&role, definition)| {
        (!matches!(role, ModelRole::Default | ModelRole::Plan)).then_some((role, definition))
    })
}

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
                let mut def = Definition::parse(&fs::read_to_string(&path)?)?;
                ensure!(
                    !ModelRole::ALL
                        .iter()
                        .any(|role| def.name.eq_ignore_ascii_case(role.name())),
                    "custom role name is reserved"
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_markdown_keeps_tools_optional_and_parses_only_the_frontmatter() {
        let markdown = "---\r\nname: scout\r\ndescription: Investigate\r\nreadonly: true\r\n---\r\n\r\nReview.\r\n---\r\nKeep this in the prompt.";
        let role = Definition::parse(markdown).unwrap();
        assert!(role.readonly);
        assert!(role.tools.is_none());
        assert_eq!(role.prompt, "Review.\n---\nKeep this in the prompt.");

        let role = Definition::parse(
            "---\nname: old\ndescription: Legacy\ntools: []\nmodel: fixture/old\nthinking: low\nspawns: []\nblocking: true\noutput: {type: object}\n---\nBody.",
        ).unwrap();
        assert!(!role.readonly);
        assert_eq!(role.tools, Some(vec![]));
        assert_eq!(role.model.as_deref(), Some("fixture/old"));
        assert_eq!(role.thinking, Some(pp_core::model::ThinkLevel::Low));
        assert!(role.blocking);
        assert_eq!(role.output.unwrap()["type"], "object");
    }

    #[test]
    fn role_markdown_rejects_invalid_frontmatter() {
        for markdown in [
            "---\nname: scout\n---\nBody",
            "---\nname: scout\ndescription: Scout\nreadonly: \"true\"\n---\nBody",
            "---\nname: scout\ndescription: Scout\nmode: execute\n---\nBody",
            "---\nname: scout\ndescription: Scout\nreadonly: true\nreadonly: false\n---\nBody",
            "---\nname: scout\ndescription: Scout\n---not-a-delimiter\nBody",
        ] {
            assert!(Definition::parse(markdown).is_err(), "{markdown}");
        }
    }
}
