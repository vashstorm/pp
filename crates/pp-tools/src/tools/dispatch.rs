//! `execute_inner` 各工具分支的实现体：父模块 `tools` 只保留门禁与薄分发。
//!
//! 子模块用 `impl Tools` 块访问祖先模块的私有字段与方法，不新增 getter。
//! `bash`／`edit`／`read` 的模块名加 `_tool` 后缀，避免遮蔽 `tools::bash`／`tools::edit`／`tools::read`。

mod ask;
mod bash_tool;
mod edit_tool;
mod eval_tool;
mod read_tool;
mod subagent;
mod web_search_tool;
mod write;

use super::*;

pub(in crate::tools) struct MutationTarget {
    pub(in crate::tools) record_key: PathBuf,
    pub(in crate::tools) is_external: bool,
    pub(in crate::tools) target: save::Target,
}

impl Tools {
    /// 解析并授权 `write` / `edit` 目标路径：支持工作区普通文件、`/tmp`、
    /// 受保护目录（`.git` / `.agents` / `.pp`，需审批）及工作区外绝对路径（`Full Access` 免审，其他模式需审批）。
    #[allow(clippy::too_many_arguments)]
    pub(in crate::tools) fn prepare_mutation_target(
        &self,
        tool: &str,
        raw_path: &str,
        create_parents: bool,
        permissions: pp_config::config::PermissionMode,
        sandbox: Option<pp_sandbox::SandboxView<'_>>,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<MutationTarget> {
        let managed = self.workspace.key(Path::new(raw_path));
        let (record_key, abs_path, is_external) = match managed {
            Ok(key) => {
                let abs = if key.is_absolute() {
                    key.clone()
                } else {
                    self.cwd.join(&key)
                };
                (key, abs, false)
            }
            Err(err) => {
                let candidate = Path::new(raw_path);
                if !candidate.is_absolute() {
                    return Err(err);
                }
                let abs = lexically_clean_abs(candidate);
                (abs.clone(), abs, true)
            }
        };

        ensure!(
            !self.permission.protects_path(&abs_path),
            "permission rules must be managed with /permissions"
        );

        let policy = match sandbox {
            Some(view) => view.policy()?,
            None => None,
        };
        let fallback_protected = protected_ancestor(&self.cwd, &abs_path);
        let sandbox_protected = policy
            .as_ref()
            .and_then(|p| match p.classify_write(&abs_path) {
                pp_sandbox::WriteAccessCheck::NeedsProtectedApproval(root) => Some(root),
                _ => None,
            });
        let effective_kind = if let Some(policy) = &policy {
            match policy.classify_write(&abs_path) {
                pp_sandbox::WriteAccessCheck::ImmutableDenied(msg) => bail!("{msg}"),
                pp_sandbox::WriteAccessCheck::NeedsProtectedApproval(_) => {
                    permission::PathWriteKind::Protected(
                        sandbox_protected
                            .as_deref()
                            .expect("checked NeedsProtectedApproval"),
                    )
                }
                pp_sandbox::WriteAccessCheck::NeedsOutsideApproval(_) => {
                    permission::PathWriteKind::OutsideWorkspace
                }
                pp_sandbox::WriteAccessCheck::NeedsReadOnlyApproval(_) => {
                    permission::PathWriteKind::WorkspaceRegular
                }
                pp_sandbox::WriteAccessCheck::Allowed => {
                    if is_external {
                        permission::PathWriteKind::OutsideWorkspace
                    } else {
                        permission::PathWriteKind::WorkspaceRegular
                    }
                }
            }
        } else if let Some(root) = fallback_protected.as_deref() {
            permission::PathWriteKind::Protected(root)
        } else if is_external {
            permission::PathWriteKind::OutsideWorkspace
        } else {
            permission::PathWriteKind::WorkspaceRegular
        };

        self.permission.authorize_path_write(
            permissions,
            tool,
            &self.cwd,
            &abs_path,
            effective_kind,
            hook_ask,
            cancellation,
        )?;

        let target = if is_external {
            self.workspace.external_target(&abs_path, create_parents)?
        } else {
            self.workspace.target(Path::new(raw_path), create_parents)?
        };

        Ok(MutationTarget {
            record_key,
            is_external,
            target,
        })
    }

    pub(super) fn execute_inner(
        &self,
        call: &mut ExecuteCall<'_, '_>,
        hook: Option<&dyn ToolHookCall>,
        hook_ask: Option<&str>,
        images: &mut Vec<read::ToolImage>,
    ) -> Result<(Value, Option<PlanChange>, Option<todo::TodoChange>)> {
        let access = self.authorize_call(call, hook_ask)?;
        let sandbox = access.sandbox;
        if let Some(value) = self.dispatch_subagent(
            call.name,
            call.arguments,
            call.call_id,
            &call.policy,
            call.cancellation,
            call.emit,
        )? {
            return Ok((value, None, None));
        }
        let file_sandbox = (access.permissions
            != pp_config::config::PermissionMode::DangerFullAccess)
            .then_some(sandbox);
        match call.name {
            "yield" => {
                let value: Value = serde_json::from_str(call.arguments)?;
                let data = value.get("data").context("yield requires data")?;
                validate_output(
                    self.yield_schema.as_ref().context("yield unavailable")?,
                    data,
                )?;
                *self.yielded.borrow_mut() = Some(data.clone());
                Ok(json!({"ok":true,"completed":true}))
            }
            "ask" => self.run_ask(call.arguments, call.call_id, call.cancellation),
            "read" => {
                return self.run_read(
                    call.arguments,
                    call.cancellation,
                    call.web,
                    images,
                    access.permissions,
                    sandbox,
                    access.hook_ask,
                );
            }
            "edit" => self.run_edit(
                call.arguments,
                call.call_id,
                call.cancellation,
                call.emit,
                access.permissions,
                file_sandbox,
                access.hook_ask,
            ),
            "write" => {
                return self.run_write(call, access.permissions, file_sandbox, access.hook_ask);
            }
            "bash" => self.run_bash(call, access.permissions, sandbox, access.hook_ask),
            "bash_task" => self.run_bash_task(call.arguments, call.call_id, call.emit),
            "eval" => return self.run_eval(call, hook, images, sandbox),
            "rg" => rg::run_with_sandbox(
                &self.cwd,
                self.limits.rg,
                self.edit_mode,
                &self.workspace,
                &mut self.snapshots.borrow_mut(),
                call.arguments,
                call.cancellation,
                Some(&self.artifacts),
                sandbox,
            ),
            "glob" => glob::run_with_sandbox(&self.cwd, call.arguments, call.cancellation, sandbox),
            "web_search" => {
                return self.run_web_search(
                    call.arguments,
                    call.cancellation,
                    call.web,
                    &call.policy,
                    access.permissions,
                    sandbox,
                    access.hook_ask,
                );
            }
            "lsp" => pp_lsp::tool::run(
                self,
                call.arguments,
                call.call_id,
                access.lsp_access,
                call.cancellation,
                call.emit,
            ),
            "find" | "plan_find" => bail!("{} is no longer available; use glob", call.name),
            "plan_submit" => {
                // Only ToolPolicy::Plan passes the gate for plan tools; the
                // outcome is reported to the caller instead of mutating any
                // workflow state here.
                let ToolPolicy::Plan {
                    document, input, ..
                } = call.policy
                else {
                    unreachable!("plan tools are gated to ToolPolicy::Plan");
                };
                return plan::run(&self.workspace, document, input, call.name, call.arguments)
                    .map(|(value, change)| (value, change, None));
            }
            "todo" => {
                return self
                    .run_todo(&call.policy, call.arguments)
                    .map(|(value, change)| (value, None, change));
            }
            "mcp_search" => super::mcp_proxy::run_search(
                &self.mcp,
                access.mcp_access,
                self.allowed_tools.as_ref(),
                call.arguments,
            ),
            "mcp_call" => {
                let resolved = access
                    .resolved_mcp_call
                    .as_ref()
                    .context("unresolved mcp_call target")?;
                if self.mcp_shared {
                    self.mcp.proxy_call(
                        &resolved.exposed_name,
                        &resolved.inner_arguments,
                        access.mcp_access,
                        call.cancellation,
                    )
                } else {
                    self.mcp.call(
                        &resolved.exposed_name,
                        &resolved.inner_arguments,
                        access.mcp_access,
                        call.cancellation,
                    )
                }
            }
            _ => {
                if self.mcp_shared {
                    self.mcp.proxy_call(
                        call.name,
                        call.arguments,
                        access.mcp_access,
                        call.cancellation,
                    )
                } else {
                    self.mcp.call(
                        call.name,
                        call.arguments,
                        access.mcp_access,
                        call.cancellation,
                    )
                }
            }
        }
        .map(|value| (value, None, None))
    }
}

fn protected_ancestor(cwd: &Path, abs_path: &Path) -> Option<PathBuf> {
    let rel = abs_path.strip_prefix(cwd).ok()?;
    let mut comps = rel.components();
    let Some(std::path::Component::Normal(first)) = comps.next() else {
        return None;
    };
    if matches!(first.to_str(), Some(".git" | ".agents" | ".pp")) {
        Some(cwd.join(first))
    } else {
        None
    }
}

fn lexically_clean_abs(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => out.push(comp.as_os_str()),
        }
    }
    out
}
