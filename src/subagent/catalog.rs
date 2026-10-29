use super::Settings;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Definition {
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

pub(crate) fn discover(cwd: &Path, user: &Path, settings: &Settings) -> Result<Vec<Definition>> {
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
                let text = text.strip_prefix("---\n").context("缺少 frontmatter")?;
                let (meta, body) = text.split_once("\n---").context("frontmatter 未结束")?;
                let mut def: Definition = crate::yaml::parse(meta, "agent definition")?;
                ensure!(
                    !def.name.trim().is_empty() && !def.description.trim().is_empty(),
                    "角色名称和描述不能为空"
                );
                ensure!(
                    def.name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                    "角色名称无效"
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
                    tracing::warn!(path = %path.display(), error = %error, "跳过无效子 Agent 定义")
                }
            }
        }
    }
    for (name, description, readonly) in [
        ("task", "执行独立的多步任务", false),
        ("scout", "只读调查与代码定位", true),
        ("reviewer", "只读审阅并报告具体问题", true),
    ] {
        found.entry(name.to_owned()).or_insert_with(|| Definition {
            name: name.to_owned(), description: description.to_owned(),
            tools: readonly.then(|| vec!["read".into(), "grep".into(), "glob".into()]),
            model: None, thinking: None,
            spawns: Some(if readonly { vec![] } else { vec!["*".into()] }),
            blocking: false, output: None,
            prompt: if readonly { "Investigate without changing files. Report evidence and file references." } else { "Complete the assigned work thoroughly. Use tools as needed. Return a concise final result." }.into(),
            source: "bundled".into(),
        });
    }
    Ok(found
        .into_values()
        .filter(|d| !settings.disabled_agents.contains(&d.name))
        .collect())
}
