//! Bash 命令专用 AST 安全门禁、`[[rules]]` 精确规则匹配与沙盒扩权派生。

use super::{ApprovedBashScope, append_load_error, policy_severity, truncate_summary};
use crate::auto_review::AutoReviewRequest;
use crate::bash_ast::{self, LeafCommand, ProgramAnalysis};
use crate::rules::{self, BashRule, FsAccess, MatchMode, SandboxGrant};
use crate::{GrantTarget, PermissionRequest, ToolAccess};
use anyhow::{Result, bail};
use pp_config::config::{PermissionMode, PermissionPolicy};
use pp_core::cancellation::CancellationToken;
use std::path::{Path, PathBuf};

const PROMPT_CMD_MAX_CHARS: usize = 2048;
const AUTO_REVIEW_CMD_MAX_CHARS: usize = 4096;

impl crate::Gate {
    /// Bash 专用完整门禁：串联 AST 危险分类、细粒度 `[[rules]]`、`.git`/`.agents` 保护、
    /// 会话/工作区外读取审批、出站网络需求、`auto_review` 审阅与 `[[sandbox]]` 作用域派生。
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_bash(
        &self,
        mode: PermissionMode,
        access: ToolAccess,
        cwd: &Path,
        arguments: &str,
        sandbox: Option<&pp_sandbox::Policy>,
        extra_writable: &[PathBuf],
        extra_session_reads: &[PathBuf],
        extra_outside_reads: &[PathBuf],
        extra_network: bool,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<ApprovedBashScope> {
        pp_core::cancellation::check(cancellation)?;
        let (doc, load_error) = self.load_store();
        let policy = self.effective_tool_policy("bash", doc.as_deref());
        if mode != PermissionMode::DangerFullAccess
            && matches!(policy, Some(PermissionPolicy::Deny))
        {
            bail!("bash is blocked by permission policy.");
        }
        let analysis = (mode != PermissionMode::DangerFullAccess)
            .then(|| bash_ast::analyze_bash_arguments(arguments))
            .flatten();

        let ast_reads = sandbox
            .zip(analysis.as_ref())
            .map(|(sandbox, analysis)| analysis.classify_read_targets(cwd, sandbox))
            .unwrap_or_default();
        if let Some(denied) = ast_reads.immutable_denied {
            bail!("{denied}");
        }

        let mut session_reads = ast_reads.session_reads;
        session_reads.extend_from_slice(extra_session_reads);
        session_reads.sort_unstable();
        session_reads.dedup();

        let mut outside_reads = ast_reads.outside_reads;
        outside_reads.extend_from_slice(extra_outside_reads);
        outside_reads.sort_unstable();
        outside_reads.dedup();

        if mode != PermissionMode::DangerFullAccess {
            for path in &outside_reads {
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

        let unapproved_outside_reads = if mode == PermissionMode::DangerFullAccess {
            Vec::new()
        } else {
            self.unapproved_read_paths(doc.as_deref(), cwd, &outside_reads)
        };

        let unapproved_session_reads =
            self.unapproved_session_reads(doc.as_deref(), &session_reads)?;
        let raw_command = extract_bash_command(arguments).unwrap_or_default();
        let (prompt_cmd, _) = format_command_for_prompt(&raw_command);

        let read_reason = (!unapproved_session_reads.is_empty()).then(|| {
            let paths = unapproved_session_reads
                .iter()
                .map(|path| pp_core::text::display_text(&path.display().to_string()).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            format!("Read protected session path(s): {paths}\nCommand: {prompt_cmd}")
        });
        let outside_reason = (!unapproved_outside_reads.is_empty()).then(|| {
            let paths = unapproved_outside_reads
                .iter()
                .map(|path| pp_core::text::display_text(&path.display().to_string()).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            format!("Read outside workspace requires approval: {paths}")
        });

        let prompt_once = |reason: &str| {
            let mut combined = with_read_reason(reason, read_reason.as_deref());
            if let Some(outside) = &outside_reason {
                combined = format!("{outside}\n{combined}");
            }
            self.prompt_user(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason: combined,
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            )
        };
        let explicit_scope = || ApprovedBashScope {
            writable: extra_writable.to_vec(),
            network: extra_network,
            session_reads: session_reads.clone(),
            outside_reads: outside_reads.clone(),
            ..Default::default()
        };
        if let Some(reason) = hook_ask {
            prompt_once(reason)?;
            return Ok(explicit_scope());
        }
        if mode == PermissionMode::DangerFullAccess
            || (policy == Some(PermissionPolicy::Allow)
                && read_reason.is_none()
                && outside_reason.is_none())
        {
            return Ok(explicit_scope());
        }

        let Some(analysis) = analysis else {
            return self.authorize_tool_call(
                mode,
                "bash",
                arguments,
                access,
                cwd,
                None,
                cancellation,
            );
        };
        if let Some(reason) = analysis.critical_reason {
            prompt_once(reason)?;
            return Ok(explicit_scope());
        }
        if matches!(policy, Some(PermissionPolicy::Prompt)) {
            prompt_once("required by the permission policy for this tool.")?;
            return Ok(explicit_scope());
        }
        let mut approved = self.enforce_bash_analysis(
            mode,
            access,
            cwd,
            arguments,
            analysis,
            extra_writable,
            &outside_reads,
            &unapproved_outside_reads,
            &unapproved_session_reads,
            extra_network,
            read_reason.as_deref(),
            doc,
            load_error,
            cancellation,
        )?;
        approved.session_reads = session_reads;
        approved.outside_reads = outside_reads;
        Ok(approved)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn enforce_bash_analysis(
        &self,
        mode: PermissionMode,
        access: ToolAccess,
        cwd: &Path,
        arguments: &str,
        analysis: ProgramAnalysis,
        extra_writable: &[PathBuf],
        outside_reads: &[PathBuf],
        unapproved_outside_reads: &[PathBuf],
        unapproved_session_reads: &[PathBuf],
        extra_network: bool,
        read_reason: Option<&str>,
        doc: Option<std::sync::Arc<rules::Document>>,
        load_error: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<ApprovedBashScope> {
        let saved_rules = doc.as_ref().map_or(&[][..], |d| d.rules.as_slice());

        let mut forced_prompt_patterns = Vec::new();
        for leaf in &analysis.leaves {
            if let Some(rule) = matching_bash_rule(leaf, saved_rules) {
                match rule.policy {
                    PermissionPolicy::Deny => {
                        bail!(
                            "bash command is blocked by saved permission rule: {}",
                            rule.display_pattern()
                        );
                    }
                    PermissionPolicy::Prompt => {
                        let display = rule.display_pattern();
                        if !forced_prompt_patterns.contains(&display) {
                            forced_prompt_patterns.push(display);
                        }
                    }
                    PermissionPolicy::Allow => {}
                }
            }
        }
        if !forced_prompt_patterns.is_empty() {
            let mut reason = with_read_reason(
                &format!(
                    "Command matched a prompt permission rule ({}).",
                    forced_prompt_patterns.join("; ")
                ),
                read_reason,
            );
            if !unapproved_outside_reads.is_empty() {
                let paths = unapproved_outside_reads
                    .iter()
                    .map(|p| pp_core::text::display_text(&p.display().to_string()).into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                reason = format!("Read outside workspace requires approval: {paths}\n{reason}");
            }
            self.prompt_user(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason,
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                load_error.as_deref(),
                cancellation,
            )?;
            return Ok(ApprovedBashScope {
                writable: extra_writable.to_vec(),
                network: extra_network,
                outside_reads: outside_reads.to_vec(),
                ..Default::default()
            });
        }

        // 筛选确实处于保护状态的写目标（如果在 permission.toml 中显式允许写则不拦截）
        let mut protected_targets = analysis.protected_write_targets(cwd);
        protected_targets.retain(|path| {
            !matches!(
                self.lookup_fs_access(doc.as_deref(), cwd, path),
                Some(FsAccess::Write)
            )
        });
        let needs_network = !access.network_enabled && (extra_network || analysis.needs_network());
        let mut writable_sorted = extra_writable.to_vec();
        writable_sorted.sort_unstable();
        writable_sorted.dedup();
        let mut protected_sorted = protected_targets;
        protected_sorted.sort_unstable();
        protected_sorted.dedup();

        let suggested_rules = if analysis.unanalyzable {
            None
        } else {
            ProgramAnalysis::suggested_rules(analysis.leaves.iter())
        };

        let command_key = suggested_rules
            .as_ref()
            .map(|rules| {
                rules
                    .iter()
                    .map(BashRule::display_pattern)
                    .collect::<Vec<_>>()
                    .join(" && ")
            })
            .unwrap_or_else(|| extract_bash_command(arguments).unwrap_or_default());
        let has_sandbox_needs =
            !writable_sorted.is_empty() || !protected_sorted.is_empty() || needs_network;

        let sandbox_grant = has_sandbox_needs.then(|| SandboxGrant {
            command: command_key.clone(),
            writable: writable_sorted.clone(),
            allow_protected: protected_sorted.clone(),
            network: needs_network,
        });
        let sandbox_preapproved = sandbox_grant.as_ref().is_none_or(|grant| {
            self.sandbox_grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(grant)
                || doc
                    .as_ref()
                    .is_some_and(|d| d.sandbox.iter().any(|existing| existing == grant))
        });

        let tool_granted = self.is_preapproved("bash")
            || self
                .grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains("bash")
            || self.effective_tool_policy("bash", doc.as_deref()) == Some(PermissionPolicy::Allow);

        let session_rules = self
            .bash_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let is_leaf_approved = |leaf: &LeafCommand| -> bool {
            if tool_granted {
                return true;
            }
            if !analysis.has_write_redirection && self.matches_preapproved_bash_leaf(leaf) {
                return true;
            }
            if matching_bash_rule(leaf, saved_rules)
                .is_some_and(|rule| rule.policy == PermissionPolicy::Allow)
            {
                return true;
            }
            session_rules.iter().any(|rule| leaf.matches_rule(rule))
        };

        let ask_all = mode == PermissionMode::ReadOnly || !access.sandboxed;
        let mut unapproved_leaves = Vec::new();
        let mut has_high_impact = false;
        for leaf in &analysis.leaves {
            let needs_check = leaf.is_high_impact()
                || has_sandbox_needs
                || (ask_all
                    && !(leaf.is_safe_read_only()
                        && !analysis.has_write_redirection
                        && !analysis.unanalyzable));
            if !needs_check {
                continue;
            }
            if is_leaf_approved(leaf) && sandbox_preapproved {
                continue;
            }
            if leaf.is_high_impact() {
                has_high_impact = true;
            }
            unapproved_leaves.push(leaf);
        }

        let needs_unanalyzable_prompt =
            ask_all && analysis.unanalyzable && (!tool_granted || !sandbox_preapproved);
        let needs_sandbox_prompt = has_sandbox_needs && !sandbox_preapproved;
        let has_outside_read_needs = !unapproved_outside_reads.is_empty();
        if unapproved_leaves.is_empty()
            && !needs_unanalyzable_prompt
            && !needs_sandbox_prompt
            && read_reason.is_none()
            && !has_outside_read_needs
        {
            return Ok(ApprovedBashScope {
                writable: writable_sorted,
                allow_protected: protected_sorted,
                network: needs_network,
                outside_reads: outside_reads.to_vec(),
                ..Default::default()
            });
        }

        let mut reasons = Vec::new();
        if has_outside_read_needs {
            let list = unapproved_outside_reads
                .iter()
                .map(|p| pp_core::text::display_text(&p.display().to_string()).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            reasons.push(format!("Read outside workspace requires approval: {list}"));
        }
        if !protected_sorted.is_empty() {
            let list = protected_sorted
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            reasons.push(format!("Modifies protected path(s): {list}"));
        }
        if !writable_sorted.is_empty() {
            let list = writable_sorted
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            reasons.push(format!("Extra writable directories: {list}"));
        }
        if needs_network {
            reasons.push("Requires outbound network access".to_owned());
        }
        if reasons.is_empty() && (read_reason.is_none() || has_high_impact || analysis.unanalyzable)
        {
            if has_high_impact {
                reasons.push("High-impact shell command requires permission.".to_owned());
            } else if analysis.unanalyzable {
                reasons.push(
                    "Dynamic or unanalyzable shell command requires one-time approval.".to_owned(),
                );
            } else if !access.sandboxed {
                reasons.push(
                    "bash runs outside the workspace sandbox and requires permission.".to_owned(),
                );
            } else {
                reasons.push("bash requires permission in read-only mode.".to_owned());
            }
        }
        if let Some(reason) = read_reason {
            reasons.insert(0, reason.to_owned());
            if !unapproved_leaves.is_empty() || needs_unanalyzable_prompt || needs_sandbox_prompt {
                reasons
                    .push("Remembered approval applies only to the requested read paths.".into());
            }
            self.prompt_read_paths(
                "bash",
                &reasons.join("\n"),
                unapproved_session_reads
                    .iter()
                    .chain(unapproved_outside_reads)
                    .cloned()
                    .collect(),
                load_error.as_deref(),
                cancellation,
            )?;
            return Ok(ApprovedBashScope {
                writable: writable_sorted,
                allow_protected: protected_sorted,
                network: needs_network,
                outside_reads: outside_reads.to_vec(),
                ..Default::default()
            });
        }
        let need_command_rules = !unapproved_leaves.is_empty() || needs_unanalyzable_prompt;
        let final_suggested = if !need_command_rules || analysis.unanalyzable {
            None
        } else {
            ProgramAnalysis::suggested_rules(unapproved_leaves)
        };
        if let Some(rules) = &final_suggested {
            let formatted = rules
                .iter()
                .map(BashRule::display_pattern)
                .collect::<Vec<_>>()
                .join("; ");
            let label = if rules.len() == 1 {
                "Allow rule (exact)"
            } else {
                "Allow rules (exact)"
            };
            reasons.push(format!("{label}: {formatted}"));
        }

        let command_text = extract_bash_command(arguments).unwrap_or(command_key);
        let (prompt_cmd, cmd_truncated_for_prompt) = format_command_for_prompt(&command_text);
        if cmd_truncated_for_prompt || has_outside_read_needs {
            reasons.push(format!("Command: {prompt_cmd}"));
        }

        let trigger = reasons.join("\n");
        let reason_with_err = append_load_error(&trigger, load_error.as_deref());

        let command_memorizable =
            !need_command_rules || (final_suggested.is_some() && !analysis.unanalyzable);
        let sandbox_memorizable = !has_sandbox_needs || !analysis.unanalyzable;
        let read_paths_ok = unapproved_outside_reads
            .iter()
            .all(|p| p.to_str().is_some());
        let memorizable = !cmd_truncated_for_prompt
            && command_memorizable
            && sandbox_memorizable
            && (final_suggested.is_some()
                || (sandbox_grant.is_some() && !analysis.unanalyzable)
                || has_outside_read_needs);

        let session_grant = memorizable;
        let persistent_grant = memorizable
            && self.store.is_some()
            && load_error.is_none()
            && (!has_outside_read_needs || read_paths_ok);

        let grant_target = if !memorizable {
            None
        } else if has_outside_read_needs {
            Some(GrantTarget::BashAndOutsideReads {
                rules: final_suggested,
                sandbox: sandbox_grant,
                read_paths: unapproved_outside_reads.to_vec(),
            })
        } else {
            match (final_suggested, sandbox_grant) {
                (Some(rules), Some(sandbox)) => {
                    Some(GrantTarget::BashAndSandbox { rules, sandbox })
                }
                (Some(rules), None) => Some(GrantTarget::BashRules(rules)),
                (None, Some(sandbox)) => Some(GrantTarget::Sandbox(sandbox)),
                (None, None) => None,
            }
        };

        let sanitized_cmd = pp_core::logging::diagnostic_text(&command_text);
        let skip_auto_review =
            cmd_truncated_for_prompt || command_text.chars().count() > AUTO_REVIEW_CMD_MAX_CHARS;
        if skip_auto_review {
            let reason = if mode == PermissionMode::AutoReview {
                append_load_error(
                    &format!(
                        "Auto-review skipped for oversized command; manual approval required.\n{trigger}"
                    ),
                    load_error.as_deref(),
                )
            } else {
                reason_with_err
            };
            self.prompt_user(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason,
                    session_grant,
                    persistent_grant,
                },
                grant_target,
                load_error.as_deref(),
                cancellation,
            )?;
        } else {
            self.decide_with_optional_review(
                mode,
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason: reason_with_err,
                    session_grant,
                    persistent_grant,
                },
                AutoReviewRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    mode,
                    cwd: cwd.to_path_buf(),
                    trigger_reason: trigger,
                    action_summary: truncate_summary(&sanitized_cmd, AUTO_REVIEW_CMD_MAX_CHARS),
                    requested_writable: writable_sorted.clone(),
                    requested_protected: protected_sorted.clone(),
                    requested_network: needs_network,
                },
                grant_target,
                load_error.as_deref(),
                cancellation,
            )?;
        }
        Ok(ApprovedBashScope {
            writable: writable_sorted,
            allow_protected: protected_sorted,
            network: needs_network,
            outside_reads: outside_reads.to_vec(),
            ..Default::default()
        })
    }
}

fn format_command_for_prompt(command: &str) -> (String, bool) {
    let clean =
        pp_core::text::display_text(&pp_core::logging::diagnostic_text(command)).into_owned();
    let total_chars = clean.chars().count();
    if total_chars <= PROMPT_CMD_MAX_CHARS {
        return (clean, false);
    }
    let truncated: String = clean.chars().take(PROMPT_CMD_MAX_CHARS).collect();
    let hidden = total_chars - PROMPT_CMD_MAX_CHARS;
    (
        format!(
            "{truncated}…\n[WARNING: command truncated — {hidden} additional char(s) omitted; inspect full command with Ctrl+O before approving]"
        ),
        true,
    )
}

fn with_read_reason(reason: &str, read_reason: Option<&str>) -> String {
    match read_reason {
        Some(read_reason) => format!("{read_reason}\n{reason}"),
        None => reason.to_owned(),
    }
}

fn extract_bash_command(arguments: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    value
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// 同级严重度下 `exact` 匹配优先于 `prefix` 匹配；同为 `prefix` 时最长前缀优先。
fn matching_bash_rule<'a>(leaf: &LeafCommand, rules: &'a [BashRule]) -> Option<&'a BashRule> {
    rules
        .iter()
        .filter(|rule| match rule.policy {
            PermissionPolicy::Deny | PermissionPolicy::Prompt => {
                leaf.matches_restrictive_rule(rule)
            }
            PermissionPolicy::Allow => leaf.matches_rule(rule),
        })
        .max_by_key(|rule| {
            (
                policy_severity(rule.policy),
                u8::from(rule.match_mode == MatchMode::Exact),
                rule.pattern.len(),
            )
        })
}
