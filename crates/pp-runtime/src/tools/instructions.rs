//! 项目规则的工具接入；复用保存层路径校验，不扩展任何读写权限。
use super::*;
use crate::project_instructions::{Document, State};

impl Tools {
    pub(crate) fn reset_project_instructions(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut state = State::new(&self.cwd);
        for path in paths {
            if let Some(parent) = path.parent() {
                state.remember(parent.to_owned());
            }
        }
        *self.project_instructions.borrow_mut() = Some(state);
    }

    pub(crate) fn project_documents(&self) -> Result<Vec<Document>> {
        let mut state = self.project_instructions.borrow_mut();
        state.get_or_insert_with(|| State::new(&self.cwd)).refresh()
    }

    pub(crate) fn project_scope_summary(&self) -> String {
        self.project_instructions
            .borrow()
            .as_ref()
            .map_or_else(String::new, State::scope_summary)
    }

    pub(crate) fn project_request_sent(&self) {
        if let Some(state) = self.project_instructions.borrow_mut().as_mut() {
            state.sent();
        }
    }

    pub(crate) fn project_instruction_paths(&self) -> Vec<PathBuf> {
        self.project_instructions
            .borrow()
            .as_ref()
            .map_or_else(Vec::new, State::paths)
    }

    pub(crate) fn instruction_batch_blocked(&self) -> Option<String> {
        self.project_instructions
            .borrow()
            .as_ref()
            .and_then(|state| state.blocked().map(str::to_owned))
    }

    pub(crate) fn prepare_instruction_batch<'a>(
        &self,
        calls: impl IntoIterator<Item = (&'a str, &'a str)>,
        mode: WorkMode,
    ) {
        if mode != WorkMode::Execute || self.project_instructions.borrow().is_none() {
            return;
        }
        for (name, arguments) in calls {
            if self
                .allowed_tools
                .as_ref()
                .is_some_and(|allowed| !allowed.iter().any(|tool| tool == name))
            {
                continue;
            }
            // 非法参数、越界和符号链接仍由原工具返回原错误；预检不创建目录。
            let Ok(Some(path)) = self.mutation_path(name, arguments) else {
                continue;
            };
            let mut state = self.project_instructions.borrow_mut();
            let Some(state) = state.as_mut() else {
                return;
            };
            if let Err(error) = state.check(path.parent().unwrap_or(&self.cwd).to_owned()) {
                state.block(format!("{error:#}"));
            }
        }
    }

    fn mutation_path(&self, name: &str, arguments: &str) -> Result<Option<PathBuf>> {
        let path = match name {
            "edit" => edit::Request::parse(self.edit_mode, arguments)?
                .path()
                .to_owned(),
            "write" => serde_json::from_str::<WriteArgs>(arguments)?.path,
            _ => return Ok(None),
        };
        if self.subagent.is_some() && crate::subagent::is_uri(&path)
            || read::is_pipe_uri(&path)
            || path.starts_with("artifact://")
        {
            return Ok(None);
        }
        let key = self.workspace.key(Path::new(&path))?;
        self.workspace.resource_target(Path::new(&path), false)?;
        Ok(Some(self.cwd.join(key)))
    }

    pub(super) fn check_mutation_instructions(&self, name: &str, arguments: &str) -> Result<()> {
        if self.project_instructions.borrow().is_none() {
            return Ok(());
        }
        if let Some(path) = self.mutation_path(name, arguments)? {
            let mut state = self.project_instructions.borrow_mut();
            if let Some(state) = state.as_mut() {
                let result = state.check(path.parent().unwrap_or(&self.cwd).to_owned());
                if let Err(error) = &result {
                    state.block(format!("{error:#}"));
                }
                result?;
            }
        }
        Ok(())
    }

    pub(super) fn activate_read_instructions(&self, path: &str) -> Result<()> {
        if self.project_instructions.borrow().is_none() {
            return Ok(());
        }
        if let Some(path) = read::local_path(path, &self.cwd)? {
            let path = path
                .canonicalize()
                .with_context(|| format!("resolving local read path: {}", path.display()))?;
            let directory = if path.is_dir() {
                path
            } else {
                path.parent()
                    .context("read requires a parent directory")?
                    .to_owned()
            };
            if let Some(state) = self.project_instructions.borrow_mut().as_mut() {
                state.activate(directory)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn execute(tools: &Tools, name: &str, args: Value) -> Value {
        let workflow = WorkflowState::default();
        let execution = tools.execute(
            name,
            &args.to_string(),
            "instructions-call",
            ToolPolicy::Execute {
                workflow: &workflow,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(execution.stop_error.is_none());
        serde_json::from_str(&execution.output).unwrap()
    }

    #[test]
    fn project_instructions_hashline_requires_rules_in_generating_request() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::create_dir(root.join("child")).unwrap();
        fs::write(root.join("child/AGENTS.md"), "child-rule").unwrap();
        fs::write(root.join("child/code"), "before\n").unwrap();
        let tools = Tools::new(root.to_owned(), EditMode::Hashline).unwrap();
        tools.reset_project_instructions([]);
        tools.project_documents().unwrap();
        tools.project_request_sent();
        let output = execute(&tools, "read", json!({"path":"child/code:1-1"}));
        assert_eq!(output["ok"], true, "{output}");
        let header = output["content"].as_str().unwrap().lines().next().unwrap();
        let args = json!({"input":format!("{header}\nPUT 1.=1:\n+after")});
        let blocked = execute(&tools, "edit", args.clone());
        assert_eq!(blocked["ok"], false, "{blocked}");
        assert!(blocked["error"].as_str().unwrap().contains("Not executed:"));
        assert_eq!(
            fs::read_to_string(root.join("child/code")).unwrap(),
            "before\n"
        );
        tools.project_documents().unwrap();
        tools.project_request_sent();
        let output = execute(&tools, "edit", args);
        assert_eq!(output["ok"], true, "{output}");
        assert_eq!(
            fs::read_to_string(root.join("child/code")).unwrap(),
            "after\n"
        );
    }

    #[test]
    fn project_instructions_path_normalization_cannot_skip_rules() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::create_dir_all(root.join("child/a:")).unwrap();
        fs::write(root.join("child/AGENTS.md"), "child-rule").unwrap();
        let tools = Tools::new(root.to_owned(), EditMode::Replace).unwrap();
        tools.reset_project_instructions([]);
        tools.project_documents().unwrap();
        tools.project_request_sent();
        for path in ["./child/../child/a://file", "child/a:/file"] {
            let output = execute(&tools, "write", json!({"path":path,"content":"blocked"}));
            assert_eq!(output["ok"], false, "{output}");
            assert!(!root.join("child/a:/file").exists());
        }
        tools.project_documents().unwrap();
        tools.project_request_sent();
        let output = execute(
            &tools,
            "write",
            json!({"path":"child/a://file","content":"allowed"}),
        );
        assert_eq!(output["ok"], true, "{output}");
    }

    #[test]
    fn project_instructions_external_read_scopes_and_write_permissions_are_independent() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path();
        let root = parent.join("main");
        let external = parent.join("external");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(external.join(".git")).unwrap();
        fs::write(root.join("AGENTS.md"), "main-rule").unwrap();
        fs::write(external.join("AGENTS.md"), "external-rule").unwrap();
        fs::write(external.join("code"), "external-content").unwrap();
        fs::write(parent.join("AGENTS.md"), "unrelated-rule").unwrap();
        fs::write(parent.join("loose"), "loose-content").unwrap();
        let tools = Tools::new(root, EditMode::Replace).unwrap();
        tools.reset_project_instructions([]);
        tools.project_documents().unwrap();
        tools.project_request_sent();
        for path in [external.join("code"), parent.join("loose")] {
            let output = execute(&tools, "read", json!({"path":path}));
            assert_eq!(output["ok"], true, "{output}");
        }
        let documents = tools.project_documents().unwrap();
        let text = crate::project_instructions::render(&documents);
        assert!(text.contains("main-rule") && text.contains("external-rule"));
        assert!(!text.contains("unrelated-rule"));
        tools.project_request_sent();
        // 相对路径不能通过逃逸启动目录获得临时根的写权限。
        let output = execute(
            &tools,
            "write",
            json!({"path":"../external/code","content":"forbidden"}),
        );
        assert_eq!(output["ok"], false, "{output}");
        assert_eq!(
            fs::read_to_string(external.join("code")).unwrap(),
            "external-content"
        );
    }
}
