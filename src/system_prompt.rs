//! 系统指令来源快照、模板渲染和按块拼装。
use crate::{
    config::{EditMode, ToolLimits},
    project_instructions,
};
use anyhow::{Context, Result, anyhow, ensure};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use std::path::Path;

const BUNDLED: &str = include_str!("prompts/system.md");
const RUNTIME: &str = include_str!("prompts/runtime.md");

/// 用于区分不可降级的提示词启动错误和可交互修复的模型配置错误。
#[derive(Debug)]
pub(crate) struct PromptFailure;
impl std::fmt::Display for PromptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("system prompt initialization failed")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourceKind {
    Bundled,
    Text,
    Template,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    kind: SourceKind,
    content: String,
    origin: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    version: u32,
    base: Source,
    append: Option<Source>,
}

/// 用户模板的第一版公开上下文，不包含自动生成的指令块。
#[derive(Serialize)]
pub(crate) struct RenderContext<'a> {
    pub cwd: String,
    pub date: String,
    pub model: &'a str,
    pub os: &'static str,
    pub arch: &'static str,
    pub edit_mode: &'static str,
    pub work_mode: &'a str,
    pub tools: Vec<String>,
}

impl<'a> RenderContext<'a> {
    pub fn new(
        cwd: &Path,
        model: &'a str,
        edit: EditMode,
        mode: &'a str,
        tools: Vec<String>,
    ) -> Self {
        Self {
            cwd: cwd.display().to_string(),
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            model,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            edit_mode: match edit {
                EditMode::Replace => "replace",
                EditMode::Hashline => "hashline",
            },
            work_mode: mode,
            tools,
        }
    }
}

#[derive(Default)]
pub(crate) struct Blocks(Vec<String>);
impl Blocks {
    pub fn push(&mut self, text: String) {
        if !text.trim().is_empty() {
            self.0.push(text);
        }
    }
    pub fn finish(self) -> String {
        self.0.join("\n\n")
    }
}

fn renderer() -> Handlebars<'static> {
    let mut renderer = Handlebars::new();
    renderer.set_strict_mode(true);
    renderer.register_escape_fn(handlebars::no_escape);
    renderer
}

impl Snapshot {
    pub(crate) fn bundled() -> Self {
        Self {
            version: 1,
            base: Source {
                kind: SourceKind::Bundled,
                content: BUNDLED.into(),
                origin: "bundled system prompt".into(),
            },
            append: None,
        }
    }

    pub fn discover(
        cwd: &Path,
        global: &Path,
        text: Option<&str>,
        template: Option<&str>,
        append: Option<&str>,
    ) -> Result<Self> {
        ensure!(
            text.is_none() || template.is_none(),
            "--system-prompt and --system-prompt-template cannot be combined"
        );
        let mut snapshot = Self::bundled();
        if let Some(input) = text {
            snapshot.base = resolve_text(cwd, input, "--system-prompt")?;
        } else if let Some(path) = template {
            let path = cwd.join(path);
            snapshot.base = Source {
                kind: SourceKind::Template,
                content: project_instructions::read_file(&path)?.with_context(|| {
                    format!("system prompt template not found: {}", path.display())
                })?,
                origin: path.display().to_string(),
            };
        } else {
            'discovery: for dir in [cwd.join(".pp"), global.to_owned()] {
                for (name, kind) in [
                    ("SYSTEM.md", SourceKind::Text),
                    ("SYSTEM_TEMPLATE.md", SourceKind::Template),
                ] {
                    let path = dir.join(name);
                    if let Some(content) = project_instructions::read_file(&path)? {
                        snapshot.base = Source {
                            kind,
                            content,
                            origin: path.display().to_string(),
                        };
                        break 'discovery;
                    }
                }
            }
        }
        if matches!(snapshot.base.kind, SourceKind::Text) && snapshot.base.content.trim().is_empty()
        {
            snapshot.base.kind = SourceKind::Bundled;
            snapshot.base.content = BUNDLED.into();
        }
        if let Some(input) = append {
            snapshot.append = Some(resolve_text(cwd, input, "--append-system-prompt")?);
        } else {
            for dir in [cwd.join(".pp"), global.to_owned()] {
                let path = dir.join("APPEND_SYSTEM.md");
                if let Some(content) = project_instructions::read_file(&path)? {
                    snapshot.append = Some(Source {
                        kind: SourceKind::Text,
                        content,
                        origin: path.display().to_string(),
                    });
                    break;
                }
            }
        }
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported system prompt snapshot version"
        );
        if !matches!(self.base.kind, SourceKind::Text) {
            self.base_renderer()?;
        }
        if let Some(append) = &self.append {
            ensure!(
                matches!(append.kind, SourceKind::Text),
                "append prompt must be plain text: {}",
                append.origin
            );
        }
        Ok(())
    }

    fn base_renderer(&self) -> Result<Handlebars<'static>> {
        ensure!(
            !self.base.content.trim().is_empty(),
            "system prompt template must not be empty: {}",
            self.base.origin
        );
        let mut renderer = renderer();
        renderer
            .register_template_string("base", &self.base.content)
            .map_err(|error| {
                // 库的完整错误会附带源码片段；这里只保留来源、位置和语法原因。
                let reason = match error.reason() {
                    handlebars::TemplateErrorReason::InvalidSyntax(_) => {
                        "invalid Handlebars syntax".to_owned()
                    }
                    reason => reason.to_string(),
                };
                let position = error
                    .pos()
                    .map(|(line, column)| format!(" at line {line}, column {column}"))
                    .unwrap_or_default();
                anyhow!(
                    "invalid system prompt template: {}{position}: {reason}",
                    self.base.origin
                )
            })?;
        Ok(renderer)
    }

    pub fn compose(
        &self,
        context: &RenderContext<'_>,
        limits: ToolLimits,
        project: &str,
        skills: &str,
        workflow: &str,
    ) -> Result<String> {
        let base = match self.base.kind {
            SourceKind::Text => self.base.content.clone(),
            _ => self
                .base_renderer()?
                .render("base", context)
                .with_context(|| {
                    format!("rendering system prompt template: {}", self.base.origin)
                })?,
        };
        let mut runtime = serde_json::to_value(context)?;
        for name in ["read", "edit", "write", "bash", "grep", "glob"] {
            runtime[format!("has_{name}")] = context.tools.iter().any(|tool| tool == name).into();
        }
        runtime["has_writes"] =
            (runtime["has_write"] == true || runtime["has_edit"] == true).into();
        runtime["hashline"] = (context.edit_mode == "hashline").into();
        runtime["read_max_bytes"] = limits.read_max_bytes.into();
        runtime["read_max_lines"] = limits.read_max_lines.into();
        runtime["bash_timeout_seconds"] = limits.bash_timeout_seconds.into();
        let mut blocks = Blocks::default();
        blocks.push(base);
        blocks.push(
            renderer()
                .render_template(RUNTIME, &runtime)?
                .trim()
                .to_owned(),
        );
        blocks.push(
            project_instructions::compose("", Path::new(&context.cwd), project, skills)
                .trim()
                .to_owned(),
        );
        if let Some(append) = &self.append {
            blocks.push(append.content.clone());
        }
        blocks.push(workflow.to_owned());
        Ok(blocks.finish())
    }
}

fn resolve_text(cwd: &Path, input: &str, flag: &str) -> Result<Source> {
    if !input.contains('\n') && !input.is_empty() {
        let path = cwd.join(input);
        match project_instructions::read_file(&path) {
            Ok(Some(content)) => {
                return Ok(Source {
                    kind: SourceKind::Text,
                    content,
                    origin: path.display().to_string(),
                });
            }
            Ok(None) => {}
            Err(error)
                if error.downcast_ref::<rustix::io::Errno>()
                    == Some(&rustix::io::Errno::NAMETOOLONG) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Source {
        kind: SourceKind::Text,
        content: input.into(),
        origin: flag.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn render(source: &Snapshot, cwd: &Path) -> Result<String> {
        source.compose(
            &RenderContext::new(
                cwd,
                "model<&>",
                EditMode::Replace,
                "execute",
                vec!["read".into(), "edit".into(), "bash".into()],
            ),
            ToolLimits::default(),
            "project {{unknown}}",
            "skill marker",
            "workflow marker",
        )
    }

    #[test]
    fn system_prompt_discovery_priority_and_no_ancestor_or_foreign_lookup() {
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let candidates = [
            (project.path(), ".pp/SYSTEM.md", "project text"),
            (
                project.path(),
                ".pp/SYSTEM_TEMPLATE.md",
                "project {{model}}",
            ),
            (global.path(), "SYSTEM.md", "global text"),
            (global.path(), "SYSTEM_TEMPLATE.md", "global {{model}}"),
        ];
        for (root, name, text) in candidates {
            write(root, name, text);
        }
        for (root, name, expected) in candidates {
            let source =
                Snapshot::discover(project.path(), global.path(), None, None, None).unwrap();
            assert_eq!(source.base.content, expected);
            fs::remove_file(root.join(name)).unwrap();
        }
        write(project.path(), ".pp/SYSTEM.md", "ancestor");
        write(project.path(), "child/.codex/SYSTEM.md", "foreign");
        let child = project.path().join("child");
        assert!(matches!(
            Snapshot::discover(&child, global.path(), None, None, None)
                .unwrap()
                .base
                .kind,
            SourceKind::Bundled
        ));
    }

    #[test]
    fn system_prompt_explicit_and_empty_sources_suppress_discovery() {
        let cwd = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        write(cwd.path(), ".pp/SYSTEM.md", "project");
        write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", "{{#if broken}}");
        write(cwd.path(), ".pp/APPEND_SYSTEM.md", "project append");
        write(global.path(), "APPEND_SYSTEM.md", "global append");
        let load = |text, append| {
            Snapshot::discover(cwd.path(), global.path(), text, None, append).unwrap()
        };
        assert_eq!(load(None, None).append.unwrap().content, "project append");
        assert_eq!(load(Some("cli"), Some("cli append")).base.content, "cli");
        let source = load(Some(""), Some(""));
        let rendered = render(&source, cwd.path()).unwrap();
        assert!(rendered.starts_with("You are a coding assistant."));
        assert!(!rendered.contains("project append"));
        assert!(!rendered.contains("global append"));
        write(cwd.path(), ".pp/SYSTEM.md", " \n");
        assert!(matches!(load(None, None).base.kind, SourceKind::Bundled));
    }

    #[test]
    fn system_prompt_text_resolution_is_nonrecursive_and_templates_are_unescaped() {
        let cwd = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        write(cwd.path(), "literal.md", "text {{model}} <&>");
        write(
            cwd.path(),
            "template.md",
            "model={{model}};{{#each tools}}[{{this}}]{{/each}};mode={{work_mode}};{{#if tools}}enabled{{/if}}",
        );
        let literal = Snapshot::discover(
            cwd.path(),
            global.path(),
            Some("literal.md"),
            None,
            Some("append {{model}}"),
        )
        .unwrap();
        let result = render(&literal, cwd.path()).unwrap();
        assert!(result.starts_with("text {{model}} <&>"));
        assert!(result.contains("project {{unknown}}"));
        assert!(result.contains("append {{model}}"));
        let template =
            Snapshot::discover(cwd.path(), global.path(), None, Some("template.md"), None).unwrap();
        let result = render(&template, cwd.path()).unwrap();
        assert!(result.starts_with("model=model<&>;[read][edit][bash];mode=execute;enabled"));
        assert!(!result.contains("&lt;"));
        for input in [
            "missing.md".to_owned(),
            "a".repeat(10000),
            "multiline\n{{model}}".into(),
        ] {
            let source =
                Snapshot::discover(cwd.path(), global.path(), Some(&input), None, None).unwrap();
            assert!(render(&source, cwd.path()).unwrap().starts_with(&input));
        }
    }

    #[test]
    fn system_prompt_invalid_selected_sources_fail_without_fallback() {
        let cwd = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        write(global.path(), "SYSTEM.md", "must not fall back");
        for text in ["", " \n", "secret-source-marker {{#if model}}"] {
            write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", text);
            let error =
                Snapshot::discover(cwd.path(), global.path(), None, None, None).unwrap_err();
            assert!(format!("{error:#}").contains("SYSTEM_TEMPLATE.md"));
            assert!(!format!("{error:#}").contains("secret-source-marker"));
        }
        write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", "{{unknown}}");
        let source = Snapshot::discover(cwd.path(), global.path(), None, None, None).unwrap();
        let error = render(&source, cwd.path()).unwrap_err();
        assert!(format!("{error:#}").contains("SYSTEM_TEMPLATE.md"));
        assert!(
            Snapshot::discover(cwd.path(), global.path(), None, Some("missing.md"), None).is_err()
        );
        assert!(
            Snapshot::discover(
                cwd.path(),
                global.path(),
                Some("text"),
                Some("template.md"),
                None
            )
            .is_err()
        );
        let path = cwd.path().join(".pp/SYSTEM.md");
        fs::write(&path, [0xff]).unwrap();
        assert!(Snapshot::discover(cwd.path(), global.path(), None, None, None).is_err());
        fs::remove_file(&path).unwrap();
        symlink(global.path().join("SYSTEM.md"), &path).unwrap();
        assert!(Snapshot::discover(cwd.path(), global.path(), None, None, None).is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(Snapshot::discover(cwd.path(), global.path(), None, None, None).is_err());
        fs::remove_dir(&path).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(Snapshot::discover(cwd.path(), global.path(), None, None, None).is_err());
    }

    #[test]
    fn system_prompt_snapshot_preserves_sources_and_validates_version() {
        let cwd = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", "snapshot {{model}}");
        write(cwd.path(), ".pp/APPEND_SYSTEM.md", "snapshot append");
        let source = Snapshot::discover(cwd.path(), global.path(), None, None, None).unwrap();
        let mut saved = serde_json::to_value(source).unwrap();
        fs::remove_dir_all(cwd.path().join(".pp")).unwrap();
        let restored: Snapshot = serde_json::from_value(saved.clone()).unwrap();
        let result = render(&restored, cwd.path()).unwrap();
        assert!(result.starts_with("snapshot model<&>"));
        assert_eq!(result.matches("snapshot append").count(), 1);
        assert!(result.find("skill marker").unwrap() < result.find("snapshot append").unwrap());
        assert!(result.ends_with("workflow marker"));
        saved["version"] = 99.into();
        assert!(
            serde_json::from_value::<Snapshot>(saved.clone())
                .unwrap()
                .validate()
                .is_err()
        );
        saved["version"] = 1.into();
        saved["unexpected"] = true.into();
        assert!(serde_json::from_value::<Snapshot>(saved).is_err());
    }
}
