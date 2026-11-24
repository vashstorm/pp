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

impl crate::Gate {
    /// Bash 专用完整门禁：串联 AST 危险分类、细粒度 `[[rules]]`、`.git`/`.agents` 保护、
    /// 出站网络需求、`auto_review` 审阅与 `[[sandbox]]` 作用域派生。
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_bash(
        &self,
        mode: PermissionMode,
        access: ToolAccess,
        cwd: &Path,
        arguments: &str,
        extra_writable: &[PathBuf],
        extra_network: bool,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<ApprovedBashScope> {
        pp_core::cancellation::check(cancellation)?;
        let (doc, load_error) = self.load_store();
        let policy = self.effective_tool_policy("bash", doc.as_deref());
        if let Some(reason) = hook_ask {
            if mode != PermissionMode::DangerFullAccess
                && matches!(policy, Some(PermissionPolicy::Deny))
            {
                bail!("bash is blocked by permission policy.");
            }
            self.prompt_user(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            )?;
            return Ok(ApprovedBashScope {
                writable: extra_writable.to_vec(),
                allow_protected: Vec::new(),
                network: extra_network,
            });
        }
        if mode == PermissionMode::DangerFullAccess {
            return Ok(ApprovedBashScope {
                writable: extra_writable.to_vec(),
                allow_protected: Vec::new(),
                network: extra_network,
            });
        }
        match policy {
            Some(PermissionPolicy::Allow) => {
                return Ok(ApprovedBashScope {
                    writable: extra_writable.to_vec(),
                    allow_protected: Vec::new(),
                    network: extra_network,
                });
            }
            Some(PermissionPolicy::Deny) => bail!("bash is blocked by permission policy."),
            _ => {}
        }

        let Some(analysis) = bash_ast::analyze_bash_arguments(arguments) else {
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
            self.prompt_user(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            )?;
            return Ok(ApprovedBashScope {
                writable: extra_writable.to_vec(),
                allow_protected: Vec::new(),
                network: extra_network,
            });
        }
        if matches!(policy, Some(PermissionPolicy::Prompt)) {
            self.prompt_user(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason: "required by the permission policy for this tool.".to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            )?;
            return Ok(ApprovedBashScope {
                writable: extra_writable.to_vec(),
                allow_protected: Vec::new(),
                network: extra_network,
            });
        }
        self.enforce_bash_analysis(
            mode,
            access,
            cwd,
            arguments,
            analysis,
            extra_writable,
            extra_network,
            doc,
            load_error,
            cancellation,
        )
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
        extra_network: bool,
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
            self.prompt_user(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason: format!(
                        "Command matched a prompt permission rule ({}).",
                        forced_prompt_patterns.join("; ")
                    ),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                load_error.as_deref(),
                cancellation,
            )?;
            return Ok(ApprovedBashScope {
                writable: extra_writable.to_vec(),
                allow_protected: Vec::new(),
                network: extra_network,
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
        if unapproved_leaves.is_empty() && !needs_unanalyzable_prompt && !needs_sandbox_prompt {
            return Ok(ApprovedBashScope {
                writable: writable_sorted,
                allow_protected: protected_sorted,
                network: needs_network,
            });
        }

        let final_suggested = if analysis.unanalyzable {
            None
        } else if unapproved_leaves.is_empty() {
            suggested_rules
        } else {
            ProgramAnalysis::suggested_rules(unapproved_leaves)
        };

        let mut reasons = Vec::new();
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
        if reasons.is_empty() {
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
        let trigger = reasons.join("\n");
        let reason_with_err = append_load_error(&trigger, load_error.as_deref());
        // 只有能推导出安全静态规则的命令才允许会话/持久记忆（拒绝 git -c / 动态脚本记忆扩权）。
        let memorizable = final_suggested.is_some() && !analysis.unanalyzable;
        let session_grant = memorizable;
        let persistent_grant = memorizable && self.store.is_some() && load_error.is_none();
        let grant_target = match (final_suggested, sandbox_grant.filter(|_| memorizable)) {
            (Some(rules), Some(sandbox)) => Some(GrantTarget::BashAndSandbox { rules, sandbox }),
            (Some(rules), None) => Some(GrantTarget::BashRules(rules)),
            (None, Some(sandbox)) => Some(GrantTarget::Sandbox(sandbox)),
            (None, None) => None,
        };
        let command_text = extract_bash_command(arguments).unwrap_or(command_key);
        let sanitized_cmd = pp_core::logging::diagnostic_text(&command_text);
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
                action_summary: truncate_summary(&sanitized_cmd, 1024),
                requested_writable: writable_sorted.clone(),
                requested_protected: protected_sorted.clone(),
                requested_network: needs_network,
            },
            grant_target,
            load_error.as_deref(),
            cancellation,
        )?;
        Ok(ApprovedBashScope {
            writable: writable_sorted,
            allow_protected: protected_sorted,
            network: needs_network,
        })
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
