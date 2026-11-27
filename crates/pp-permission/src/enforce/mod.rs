//! 常规工具、文件路径写入、联网与 Bash AST 的权限决策实现。

mod bash;

use crate::auto_review::{AutoReviewDecision, AutoReviewRequest};
use crate::rules::{self, FsAccess};
use crate::{GrantTarget, PermissionRequest, ToolAccess};
use anyhow::{Result, bail};
use pp_config::config::{PermissionMode, PermissionPolicy};
use pp_core::cancellation::CancellationToken;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathWriteKind<'a> {
    WorkspaceRegular,
    Protected(&'a Path),
    OutsideWorkspace,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApprovedBashScope {
    pub writable: Vec<PathBuf>,
    pub allow_protected: Vec<PathBuf>,
    pub network: bool,
    pub session_reads: Vec<PathBuf>,
    pub outside_reads: Vec<PathBuf>,
}

impl crate::Gate {
    /// 确认由沙盒识别出的受保护会话文件读取权限。
    pub fn authorize_session_read(
        &self,
        target: &Path,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.authorize_path_reads(
            PermissionMode::WorkspaceWrite,
            "read",
            Path::new("/"),
            &[target.to_path_buf()],
            &[],
            hook_ask,
            cancellation,
        )
    }

    /// 统一的文件/目录读取审批门禁（供 `read` / `rg` / `glob` / `lsp` 共用）：
    /// - 受保护会话路径按精确路径复用只读授权，首次需要人工确认。
    /// - 普通工作区外路径按目录范围复用授权，未获批时
    ///   经可选 `auto_review` 审批（支持单次 / 本会话 / 全局只读写入 `permission.toml`）。
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_path_reads(
        &self,
        mode: PermissionMode,
        tool: &str,
        cwd: &Path,
        session_reads: &[PathBuf],
        outside_reads: &[PathBuf],
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        pp_core::cancellation::check(cancellation)?;
        if session_reads.is_empty() && outside_reads.is_empty() && hook_ask.is_none() {
            return Ok(());
        }
        let (doc, load_error) = self.load_store();
        let policy = self.effective_tool_policy(tool, doc.as_deref());
        if mode != PermissionMode::DangerFullAccess
            && matches!(policy, Some(PermissionPolicy::Deny))
        {
            bail!("{tool} is blocked by permission policy.");
        }

        if mode != PermissionMode::DangerFullAccess {
            for path in outside_reads {
                if matches!(
                    self.lookup_fs_access(doc.as_deref(), cwd, path),
                    Some(FsAccess::None)
                ) {
                    bail!(
                        "{} is blocked by permission.toml filesystem rule.",
                        path.display()
                    );
                }
            }
        }

        let format_paths = |paths: &[PathBuf]| -> String {
            paths
                .iter()
                .map(|p| {
                    pp_core::text::display_text(&pp_core::logging::diagnostic_text(
                        &p.display().to_string(),
                    ))
                    .into_owned()
                })
                .collect::<Vec<_>>()
                .join(", ")
        };

        let unapproved_session_reads =
            self.unapproved_session_reads(doc.as_deref(), session_reads)?;
        let unapproved_outside = if mode == PermissionMode::DangerFullAccess {
            Vec::new()
        } else {
            self.unapproved_read_paths(doc.as_deref(), cwd, outside_reads)
        };
        let policy_prompt = mode != PermissionMode::DangerFullAccess
            && matches!(policy, Some(PermissionPolicy::Prompt));
        let forced_prompt = policy_prompt || hook_ask.is_some();
        if unapproved_session_reads.is_empty() && unapproved_outside.is_empty() && !forced_prompt {
            return Ok(());
        }

        let mut reasons = Vec::new();
        if !unapproved_session_reads.is_empty() {
            reasons.push(format!(
                "Read protected session path(s): {}",
                format_paths(&unapproved_session_reads)
            ));
        }
        if !unapproved_outside.is_empty() {
            reasons.push(format!(
                "Read outside workspace requires approval: {}",
                format_paths(&unapproved_outside)
            ));
        }
        if let Some(hook_reason) = hook_ask {
            reasons.push(hook_reason.to_owned());
        } else if policy_prompt && reasons.is_empty() {
            reasons.push("required by the permission policy for this tool.".to_owned());
        }
        let trigger = reasons.join("\n");

        if forced_prompt {
            return self.prompt_user(
                PermissionRequest {
                    tool: tool.to_owned(),
                    agent: None,
                    reason: append_load_error(&trigger, load_error.as_deref()),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                load_error.as_deref(),
                cancellation,
            );
        }

        if !unapproved_session_reads.is_empty() {
            return self.prompt_read_paths(
                tool,
                &trigger,
                unapproved_session_reads
                    .into_iter()
                    .chain(unapproved_outside)
                    .collect(),
                load_error.as_deref(),
                cancellation,
            );
        }

        let persistent_grant = self.store.is_some()
            && load_error.is_none()
            && unapproved_outside.iter().all(|p| p.to_str().is_some());
        let summary = format!("{tool} read={}", format_paths(&unapproved_outside));
        self.decide_with_optional_review(
            mode,
            PermissionRequest {
                tool: tool.to_owned(),
                agent: None,
                reason: append_load_error(&trigger, load_error.as_deref()),
                session_grant: true,
                persistent_grant,
            },
            AutoReviewRequest {
                tool: tool.to_owned(),
                agent: None,
                mode,
                cwd: cwd.to_path_buf(),
                trigger_reason: trigger,
                action_summary: truncate_summary(&summary, 1024),
                requested_writable: Vec::new(),
                requested_protected: Vec::new(),
                requested_network: false,
            },
            Some(GrantTarget::HostReadPaths(unapproved_outside)),
            load_error.as_deref(),
            cancellation,
        )
    }

    pub(super) fn unapproved_session_reads(
        &self,
        doc: Option<&rules::Document>,
        paths: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let active = doc.and_then(|doc| doc.resolve_active_profile().ok());
        let grants = self
            .read_path_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 普通父目录的读取授权不能涵盖受保护的会话数据。
        let mut pending = Vec::new();
        for path in paths {
            let access = active
                .as_ref()
                .and_then(|profile| profile.filesystem.host.get(path.to_str()?));
            match access {
                Some(FsAccess::None) => bail!(
                    "{} is blocked by permission.toml filesystem rule.",
                    path.display()
                ),
                Some(FsAccess::Read | FsAccess::Write) => {}
                None if grants.contains(path) => {}
                None => pending.push(path.clone()),
            }
        }
        Ok(pending)
    }

    pub(super) fn prompt_read_paths(
        &self,
        tool: &str,
        reason: &str,
        paths: Vec<PathBuf>,
        load_error: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let persistent_grant = self.store.is_some()
            && load_error.is_none()
            && paths.iter().all(|path| {
                path.to_str()
                    .is_some_and(|path| rules::validate_host_fs_path(path).is_ok())
            });
        self.prompt_user(
            PermissionRequest {
                tool: tool.to_owned(),
                agent: None,
                reason: append_load_error(reason, load_error),
                session_grant: true,
                persistent_grant,
            },
            Some(GrantTarget::HostReadPaths(paths)),
            load_error,
            cancellation,
        )
    }

    pub(super) fn unapproved_read_paths(
        &self,
        doc: Option<&rules::Document>,
        cwd: &Path,
        paths: &[PathBuf],
    ) -> Vec<PathBuf> {
        let session_granted = self
            .read_path_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut out = Vec::new();
        for path in paths {
            let fs_ok = matches!(
                self.lookup_fs_access(doc, cwd, path),
                Some(FsAccess::Read | FsAccess::Write)
            );
            let session_ok = session_granted
                .iter()
                .any(|granted| path == granted || path.starts_with(granted));
            if !fs_ok && !session_ok && !out.contains(path) {
                out.push(path.clone());
            }
        }
        out
    }

    pub fn enforce(
        &self,
        name: &str,
        arguments: &str,
        tier: pp_workspace::Tier,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.enforce_for(
            self.snapshot(),
            name,
            arguments,
            ToolAccess {
                tier,
                sandboxed: false,
                network_enabled: false,
            },
            hook_ask,
            cancellation,
        )
    }

    pub fn enforce_for(
        &self,
        mode: PermissionMode,
        name: &str,
        arguments: &str,
        access: ToolAccess,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let _ =
            self.authorize_tool_call(mode, name, arguments, access, &cwd, hook_ask, cancellation)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn authorize_tool_call(
        &self,
        mode: PermissionMode,
        name: &str,
        arguments: &str,
        access: ToolAccess,
        cwd: &Path,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<ApprovedBashScope> {
        let tier = access.tier;
        let (doc, load_error) = self.load_store();
        let policy = self.effective_tool_policy(name, doc.as_deref());
        if let Some(reason) = hook_ask {
            if mode != PermissionMode::DangerFullAccess
                && matches!(policy, Some(PermissionPolicy::Deny))
            {
                bail!("{name} is blocked by permission policy.")
            }
            self.prompt_user(
                PermissionRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            )?;
            return Ok(ApprovedBashScope::default());
        }
        if mode == PermissionMode::DangerFullAccess {
            return Ok(ApprovedBashScope::default());
        }
        match policy {
            Some(PermissionPolicy::Allow) => return Ok(ApprovedBashScope::default()),
            Some(PermissionPolicy::Deny) => {
                bail!("{name} is blocked by permission policy.")
            }
            _ => {}
        }
        let bash_analysis = (name == "bash")
            .then(|| crate::bash_ast::analyze_bash_arguments(arguments))
            .flatten();
        if let Some(reason) = bash_analysis.as_ref().and_then(|a| a.critical_reason) {
            self.prompt_user(
                PermissionRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            )?;
            return Ok(ApprovedBashScope::default());
        }
        if matches!(policy, Some(PermissionPolicy::Prompt)) {
            self.prompt_user(
                PermissionRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: "required by the permission policy for this tool.".to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            )?;
            return Ok(ApprovedBashScope::default());
        }
        if let Some(analysis) = bash_analysis {
            return self.enforce_bash_analysis(
                mode,
                access,
                cwd,
                arguments,
                analysis,
                &[],
                &[],
                &[],
                false,
                None,
                None,
                doc,
                load_error,
                cancellation,
            );
        }

        let needs_prompt = match (mode, tier) {
            (PermissionMode::DangerFullAccess, _) => false,
            (PermissionMode::WorkspaceWrite | PermissionMode::AutoReview, _)
                if access.sandboxed =>
            {
                false
            }
            (
                PermissionMode::WorkspaceWrite | PermissionMode::AutoReview,
                pp_workspace::Tier::Read,
            ) => false,
            (
                PermissionMode::WorkspaceWrite | PermissionMode::AutoReview,
                pp_workspace::Tier::Write | pp_workspace::Tier::Exec,
            ) => true,
            (PermissionMode::ReadOnly, pp_workspace::Tier::Read) => false,
            (PermissionMode::ReadOnly, pp_workspace::Tier::Write | pp_workspace::Tier::Exec) => {
                true
            }
        };
        if !needs_prompt {
            return Ok(ApprovedBashScope::default());
        }
        if self.is_preapproved(name)
            || self
                .grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(name)
        {
            return Ok(ApprovedBashScope::default());
        }

        let trigger = match mode {
            PermissionMode::ReadOnly => "tool requires permission in read-only mode.",
            PermissionMode::WorkspaceWrite | PermissionMode::AutoReview => {
                "tool runs outside the workspace sandbox and requires permission."
            }
            PermissionMode::DangerFullAccess => unreachable!(),
        };
        let summary = pp_core::logging::diagnostic_text(&format!("tool={name} args={arguments}"));
        self.decide_with_optional_review(
            mode,
            PermissionRequest {
                tool: name.to_owned(),
                agent: None,
                reason: append_load_error(trigger, load_error.as_deref()),
                session_grant: true,
                persistent_grant: self.store.is_some() && load_error.is_none(),
            },
            AutoReviewRequest {
                tool: name.to_owned(),
                agent: None,
                mode,
                cwd: cwd.to_path_buf(),
                trigger_reason: trigger.to_owned(),
                action_summary: truncate_summary(&summary, 1024),
                requested_writable: Vec::new(),
                requested_protected: Vec::new(),
                requested_network: false,
            },
            Some(GrantTarget::Tool(name.to_owned())),
            load_error.as_deref(),
            cancellation,
        )?;
        Ok(ApprovedBashScope::default())
    }

    /// 写/改路径工具（`write` / `edit`）的细粒度权限门禁：
    /// 区分工作区普通文件、受保护路径（`.git` / `.agents` / `.pp`）与工作区外路径。
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_path_write(
        &self,
        mode: PermissionMode,
        tool: &str,
        cwd: &Path,
        target_path: &Path,
        kind: PathWriteKind<'_>,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        pp_core::cancellation::check(cancellation)?;
        let (doc, load_error) = self.load_store();
        let policy = self.effective_tool_policy(tool, doc.as_deref());
        if let Some(reason) = hook_ask {
            if mode != PermissionMode::DangerFullAccess
                && matches!(policy, Some(PermissionPolicy::Deny))
            {
                bail!("{tool} is blocked by permission policy.");
            }
            return self.prompt_user(
                PermissionRequest {
                    tool: tool.to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            );
        }
        if mode == PermissionMode::DangerFullAccess {
            return Ok(());
        }
        match policy {
            Some(PermissionPolicy::Deny) => bail!("{tool} is blocked by permission policy."),
            Some(PermissionPolicy::Prompt) => {
                return self.prompt_user(
                    PermissionRequest {
                        tool: tool.to_owned(),
                        agent: None,
                        reason: "required by the permission policy for this tool.".to_owned(),
                        session_grant: false,
                        persistent_grant: false,
                    },
                    None,
                    None,
                    cancellation,
                );
            }
            Some(PermissionPolicy::Allow) => {
                if matches!(kind, PathWriteKind::WorkspaceRegular) {
                    return Ok(());
                }
            }
            None => {}
        }

        // 检查 permission.toml 中的 `[profile.filesystem]` 显式路径设置
        if let Some(access) = self.lookup_fs_access(doc.as_deref(), cwd, target_path) {
            match access {
                FsAccess::None => bail!(
                    "{} is blocked by permission.toml filesystem rule.",
                    target_path.display()
                ),
                FsAccess::Write => return Ok(()),
                FsAccess::Read => {
                    // 显式设为 read 时即便是工作区普通文件也必须走审批
                }
            }
        } else if matches!(kind, PathWriteKind::WorkspaceRegular)
            && matches!(
                mode,
                PermissionMode::WorkspaceWrite | PermissionMode::AutoReview
            )
        {
            return Ok(());
        }

        // 只有在“普通工作区内路径 + ReadOnly 模式”才允许工具级 preapproved/grants 放行；
        // 受保护目录（.git/.agents/.pp）与工作区外文件必须精确路径授权！
        if matches!(kind, PathWriteKind::WorkspaceRegular)
            && (self.is_preapproved(tool)
                || self
                    .grants
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(tool))
        {
            return Ok(());
        }

        if self
            .path_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|granted| target_path == granted || target_path.starts_with(granted))
        {
            return Ok(());
        }

        let display_target = pp_core::text::display_text(&pp_core::logging::diagnostic_text(
            &target_path.display().to_string(),
        ))
        .into_owned();
        let (trigger, req_writable, req_protected, grant_target, persistent_grant) = match kind {
            PathWriteKind::Protected(root) => {
                let rel_root = root.strip_prefix(cwd).unwrap_or(root);
                let rel_display =
                    pp_core::text::display_text(&rel_root.display().to_string()).into_owned();
                let target_rel = target_path
                    .strip_prefix(cwd)
                    .ok()
                    .and_then(|r| r.to_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let persist_ok =
                    self.store.is_some() && load_error.is_none() && target_rel.is_some();
                (
                    format!(
                        "Protected path write requires approval ({rel_display}): {display_target}"
                    ),
                    Vec::new(),
                    vec![root.to_path_buf()],
                    Some(match target_rel {
                        Some(rel) => GrantTarget::WorkspacePath {
                            abs: target_path.to_path_buf(),
                            rel,
                        },
                        None => GrantTarget::HostPath(target_path.to_path_buf()),
                    }),
                    persist_ok,
                )
            }
            PathWriteKind::OutsideWorkspace => {
                let persist_ok =
                    self.store.is_some() && load_error.is_none() && target_path.to_str().is_some();
                (
                    format!("Edit outside workspace requires approval: {display_target}"),
                    vec![target_path.to_path_buf()],
                    Vec::new(),
                    Some(GrantTarget::HostPath(target_path.to_path_buf())),
                    persist_ok,
                )
            }
            PathWriteKind::WorkspaceRegular => (
                format!("Write requires approval in read-only mode: {display_target}"),
                Vec::new(),
                Vec::new(),
                Some(GrantTarget::HostPath(target_path.to_path_buf())),
                false,
            ),
        };

        self.decide_with_optional_review(
            mode,
            PermissionRequest {
                tool: tool.to_owned(),
                agent: None,
                reason: append_load_error(&trigger, load_error.as_deref()),
                session_grant: true,
                persistent_grant,
            },
            AutoReviewRequest {
                tool: tool.to_owned(),
                agent: None,
                mode,
                cwd: cwd.to_path_buf(),
                trigger_reason: trigger.clone(),
                action_summary: format!("{tool} target={display_target}"),
                requested_writable: req_writable,
                requested_protected: req_protected,
                requested_network: false,
            },
            grant_target,
            load_error.as_deref(),
            cancellation,
        )
    }

    /// 联网工具（`web_search` 与 URL `read`）在断网沙盒下的互联网访问审批。
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_network(
        &self,
        mode: PermissionMode,
        tool: &str,
        cwd: &Path,
        target_summary: &str,
        network_enabled: bool,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        pp_core::cancellation::check(cancellation)?;
        let (doc, load_error) = self.load_store();
        let policy = self.effective_tool_policy(tool, doc.as_deref());
        if let Some(reason) = hook_ask {
            if mode != PermissionMode::DangerFullAccess
                && matches!(policy, Some(PermissionPolicy::Deny))
            {
                bail!("{tool} is blocked by permission policy.");
            }
            return self.prompt_user(
                PermissionRequest {
                    tool: tool.to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            );
        }
        if mode == PermissionMode::DangerFullAccess && network_enabled {
            return Ok(());
        }
        match policy {
            Some(PermissionPolicy::Deny) => bail!("{tool} is blocked by permission policy."),
            Some(PermissionPolicy::Prompt) => {
                return self.prompt_user(
                    PermissionRequest {
                        tool: tool.to_owned(),
                        agent: None,
                        reason: "required by the permission policy for this tool.".to_owned(),
                        session_grant: false,
                        persistent_grant: false,
                    },
                    None,
                    None,
                    cancellation,
                );
            }
            Some(PermissionPolicy::Allow) => return Ok(()),
            None => {}
        }
        if network_enabled {
            return Ok(());
        }
        if let Some(doc) = &doc
            && let Ok(active) = doc.resolve_active_profile()
            && active.network.enabled == Some(true)
        {
            return Ok(());
        }
        if *self.network_grant.lock().unwrap_or_else(|e| e.into_inner())
            || self
                .grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(tool)
        {
            return Ok(());
        }
        let clean_target =
            pp_core::text::display_text(&pp_core::logging::diagnostic_text(target_summary))
                .into_owned();
        let trigger = format!(
            "Internet access ({tool}) is blocked by sandbox policy (outbound network is disabled) and requires approval: {clean_target}"
        );
        self.decide_with_optional_review(
            mode,
            PermissionRequest {
                tool: tool.to_owned(),
                agent: None,
                reason: append_load_error(&trigger, load_error.as_deref()),
                session_grant: true,
                persistent_grant: self.store.is_some() && load_error.is_none(),
            },
            AutoReviewRequest {
                tool: tool.to_owned(),
                agent: None,
                mode,
                cwd: cwd.to_path_buf(),
                trigger_reason: trigger.clone(),
                action_summary: truncate_summary(&clean_target, 512),
                requested_writable: Vec::new(),
                requested_protected: Vec::new(),
                requested_network: true,
            },
            Some(GrantTarget::Network),
            load_error.as_deref(),
            cancellation,
        )
    }

    pub(super) fn decide_with_optional_review(
        &self,
        mode: PermissionMode,
        mut request: PermissionRequest,
        review_request: AutoReviewRequest,
        target: Option<GrantTarget>,
        load_error: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if mode == PermissionMode::AutoReview
            && let Some(reviewer) = self
                .auto_review
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        {
            match reviewer(review_request, cancellation) {
                Ok(verdict) if verdict.decision == AutoReviewDecision::Pass => {
                    tracing::info!(
                        event = "auto_review_pass",
                        tool = %request.tool,
                        reason = %verdict.reason,
                    );
                    return Ok(());
                }
                Ok(verdict) => {
                    tracing::info!(
                        event = "auto_review_deny",
                        tool = %request.tool,
                        reason = %verdict.reason,
                    );
                    let detail = pp_core::text::display_text(&verdict.reason);
                    request.reason = format!(
                        "Reviewer flagged as potentially unsafe: {detail}\n{}",
                        request.reason
                    );
                }
                Err(error) => {
                    if error
                        .chain()
                        .any(|cause| cause.is::<pp_core::cancellation::Cancelled>())
                    {
                        return Err(error);
                    }
                    let msg = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                    tracing::warn!(event = "auto_review_error", error = %msg);
                    request.reason = format!(
                        "Auto-review unavailable ({msg}); manual approval required.\n{}",
                        request.reason
                    );
                }
            }
        }
        self.prompt_user(request, target, load_error, cancellation)
    }

    pub(super) fn effective_tool_policy(
        &self,
        name: &str,
        doc: Option<&rules::Document>,
    ) -> Option<PermissionPolicy> {
        let mut best = self.policy_for(name);
        if let Some(doc) = doc
            && let Ok(active) = doc.resolve_active_profile()
            && let Some(&rule_policy) = active.tools.get(name)
        {
            best = Some(match best {
                Some(existing) if policy_severity(existing) >= policy_severity(rule_policy) => {
                    existing
                }
                _ => rule_policy,
            });
        }
        best
    }

    pub(super) fn lookup_fs_access(
        &self,
        doc: Option<&rules::Document>,
        cwd: &Path,
        target: &Path,
    ) -> Option<FsAccess> {
        let doc = doc?;
        let active = doc.resolve_active_profile().ok()?;
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let mut best: Option<(usize, FsAccess)> = None;
        for (raw, &access) in &active.filesystem.host {
            if let Some(base) = rules::expand_host_fs_path(raw, home.as_deref())
                && target.starts_with(&base)
            {
                let depth = base.components().count();
                if best.is_none_or(|(d, _)| depth >= d) {
                    best = Some((depth, access));
                }
            }
        }
        for (rel, &access) in &active.filesystem.workspace {
            let base = cwd.join(rel);
            if target.starts_with(&base) {
                let depth = base.components().count();
                if best.is_none_or(|(d, _)| depth >= d) {
                    best = Some((depth, access));
                }
            }
        }
        best.map(|(_, access)| access)
    }
}

pub(super) fn append_load_error(reason: &str, load_error: Option<&str>) -> String {
    match load_error {
        Some(message) => format!("{reason}\nSaved permissions unavailable: {message}"),
        None => reason.to_owned(),
    }
}

pub(super) fn truncate_summary(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

pub(super) fn policy_severity(policy: PermissionPolicy) -> u8 {
    match policy {
        PermissionPolicy::Deny => 3,
        PermissionPolicy::Prompt => 2,
        PermissionPolicy::Allow => 1,
    }
}
