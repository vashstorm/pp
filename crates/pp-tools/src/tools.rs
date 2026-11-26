use crate::ports::{
    HookHalt, PlanSubmitGate, SubagentHost, ToolHookCall, validate_output, yield_definition,
};
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use pp_config::config::EditMode;
use pp_config::config::ToolLimits;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use pp_core::events::AgentEvent;
use pp_core::plan::PlanDocument;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

#[cfg(test)]
use pp_core::events::OutputStream;
#[cfg(test)]
use pp_core::plan::PlanStatus;
#[cfg(test)]
use std::io::{self, Read};

mod access;
pub use access::edit_path;
mod dispatch;
mod edit;
mod glob;
mod hashline;
mod instructions;
mod lifecycle;
pub mod permission;
use lifecycle::{finalize_tool_execution, tool_metric_name};
mod lsp;
mod mcp_proxy;
mod plan;
mod rg;
pub use plan::PlanChange;
pub mod plan_catalog;
pub mod read;
mod replace;
use pp_workspace::{diff, output, save};
mod shell_fields;
use shell_fields::ModelProjection;
mod stream;
#[cfg(test)]
use pp_workspace::stream_text::StreamText;
use stream::emit_background_snapshot;
#[cfg(test)]
use stream::{PIPE_CHUNK_SIZE, drain_pipe};
pub mod todo;
mod todo_route;
pub mod wait;
pub mod web_search;

#[cfg(test)]
const OUTPUT_LIMIT: usize = 65_536;

/// 提交后同步已启动客户端的单次预算：与内联诊断等待同一上限；自动诊断开启时
/// 受管 worker 会用后台预算再次同步，自动诊断关闭时这里就是唯一的有界同步。
const POST_SAVE_SYNC: Duration = Duration::from_millis(500);
/// `write` 的目标在读取基线之后被外部创建：拒绝覆盖而不是无条件替换。
const WRITE_NEW_CONFLICT: &str = "File appeared after the write started; write refused instead of overwriting it. Read the file and use edit.";

pub struct Tools {
    pub artifacts: output::Store,
    pub ask_handler: Option<pp_core::interaction::SharedAskHandler>,
    pub permission: permission::Gate,
    pub mcp: std::sync::Arc<pp_mcp::McpManager>,
    /// 子 Agent 共享父级 manager：分发走代理调用，父连接不因子 Agent 取消而中断。
    pub mcp_shared: bool,
    pub mcp_readonly: bool,
    mcp_settings: pp_config::config::McpSettings,
    pub lsp: Option<pp_lsp::Registry>,
    /// 仅根/独立 Registry 的 owner 持有；借用父 Registry 的子 Agent 保持 `None`，
    /// Drop 时不会停止父进程。
    lsp_cleanup: Option<pp_lsp::Cleanup>,
    /// 强制只读：readonly 子 Agent 只拿到受限动作表，执行层同样拒绝写动作。
    pub lsp_readonly: bool,
    /// 保存联动与迟到诊断的本 Agent owner；子 Agent 必须各自新建，不共享 inbox。
    pub lsp_owner: pp_lsp::writethrough::Owner,
    pub subagent: Option<std::sync::Arc<dyn SubagentHost>>,
    pub allowed_tools: Option<crate::AllowedTools>,
    pub yield_schema: Option<Value>,
    pub yielded: RefCell<Option<Value>>,
    pub skills: pp_config::skills::SharedSkills,
    cwd: PathBuf,
    edit_mode: EditMode,
    workspace: save::Workspace,
    pub write_lock: std::sync::Arc<std::sync::Mutex<()>>,
    web_cache: RefCell<read::WebCache>,
    read_dedup: RefCell<read::ReadDedupCache>,
    snapshots: RefCell<hashline::Store>,
    project_instructions: RefCell<Option<crate::project_instructions::State>>,
    background_tasks: pp_bash::TaskManager,
    eval: pp_eval::Session,
    eval_enabled: bool,
    eval_interpreter: Option<String>,
    eval_available: bool,
    wait_state: RefCell<wait::State>,
    pub limits: ToolLimits,
    host_defaults: pp_workspace::host_defaults::HostDefaults,
    pipe_store: pp_bash::PipeStore,
    sandbox: std::sync::Arc<pp_sandbox::Sandbox>,
    web_search: pp_config::config::WebSearchSettings,
}

/// Separates a tool's serialized result from a process cleanup failure that
/// must stop the agent rather than be treated as an ordinary command failure.
pub struct ToolExecution {
    pub output: String,
    pub images: Vec<read::ToolImage>,
    pub stop_error: Option<anyhow::Error>,
    /// Outcome of the dedicated plan tools for this single call; the caller
    /// applies it to the shared workflow state at the batch commit boundary.
    pub plan_change: Option<PlanChange>,
    /// 该调用收集到的 hook 上下文，按产生顺序进入批次末尾的 developer 消息。
    pub hook_context: Vec<String>,
    /// pre 的拒绝已用完这块上下文；post 决定让整批停止或失败时不再执行后续调用。
    pub hook_halt: Option<HookHalt>,
    /// Outcome of one todo call (standalone list or plan projection); the
    /// caller installs it on the workflow at the batch commit boundary via
    /// `apply_todo_change`. `None` for every non-todo tool and for `view`.
    pub todo_change: Option<todo::TodoChange>,
}

/// Per-call tool authorization, borrowed from the agent's current workflow
/// for every batch. The execution-layer gate inside `execute` is
/// authoritative; hidden tool schemas never are.
#[derive(Clone, Copy)]
pub enum ToolPolicy<'a> {
    /// Normal execution with the full toolset; the borrowed workflow state
    /// classifies todo calls as standalone lists or bound plan projections.
    Execute { workflow: &'a WorkflowState },
    /// Independent plan review: read-only investigation tools only; no ask,
    /// no plan writes or shell. MCP requires explicit read-only authorization.
    Review,
    /// Plan mode: whitelisted research tools plus the dedicated plan tools.
    Plan {
        document: &'a PlanDocument,
        /// Set once this batch has successfully submitted the plan; every
        /// later call in the batch is refused with a per-call result.
        sealed: bool,
        /// Running input queue for the plan-submit boundary; `None` only in
        /// isolated tool unit tests.
        input: Option<&'a dyn PlanSubmitGate>,
    },
}

/// In-process research tools; also used by the independent reviewer.
pub(super) const PLAN_SEARCH_TOOLS: &[&str] = &["glob", "web_search"];
/// Plan-mode-only tools that mutate the review boundary.
const PLAN_WRITE_TOOLS: &[&str] = &["plan_submit"];
/// Research tools Plan mode keeps from the ordinary Execute list.
pub(super) const PLAN_BASE_TOOLS: &[&str] = &["read", "rg"];

pub(super) fn is_plan_tool(name: &str) -> bool {
    PLAN_WRITE_TOOLS.contains(&name)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

/// `write` 的提交基线：只有目标确实可读时才允许精确比较后提交。
///
/// 不可读的目标（权限、竞态替换）没有可比较的基线：沿用既有的原子替换边界，
/// diff 如实标记为 `Unavailable`，绝不伪造一份空内容当作基线。
enum WriteBaseline {
    Existing(Vec<u8>),
    Missing,
    Unavailable,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[serde(default)]
    mode: BashMode,
    #[serde(default, deserialize_with = "deserialize_bash_timeout")]
    timeout_seconds: Option<u64>,
    sandbox_permissions: Option<BashPermissions>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashPermissions {
    #[serde(default)]
    writable: Vec<std::path::PathBuf>,
    #[serde(default)]
    network: bool,
}

fn deserialize_bash_timeout<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    u64::deserialize(deserializer).map(Some)
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BashMode {
    #[default]
    Foreground,
    Background,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashTaskArgs {
    task_id: String,
    action: BashTaskAction,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum BashTaskAction {
    Status,
    Stop,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalArgs {
    language: EvalLanguage,
    code: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, deserialize_with = "deserialize_bash_timeout")]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    reset: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum EvalLanguage {
    Py,
}

const PLAN_WRITE_DESC: &str = concat!(
    "Save the complete Markdown plan using path and content. On the first save choose ",
    ".pp/drafts/<descriptive-kebab-case-name>.md; later saves must use the exact bound plan ",
    "path. Only this plan may be written in Plan mode. The display title derives from the file ",
    "name (use H2/H3 headings; omit H1). The runtime registers the plan, advances its revision, ",
    "refuses name collisions and external changes, and returns path and revision. Renaming is ",
    "only possible through /plan-list.",
);
const PLAN_SUBMIT_DESC: &str = concat!(
    "Submit the current plan for user review and approval. The submitted draft stays in ",
    ".pp/drafts/; only the local review actions move it forward. expected_revision must equal ",
    "the current plan revision. After a successful submission no further tools run in this batch; ",
    "the user reviews the plan before any execution starts. Never ask the user to approve in ",
    "chat; only plan_submit requests review.",
);
const COMMON_READ_DESC: &str = concat!(
    "Read local text/images, static URLs, skill://, artifact://<id>, and pipe://<run>[/<stream>] URIs. ",
    "Pass only `path` (put inclusive ranges in `path`, never offset/limit): `file:20-60`, `:20` (from line 20), `:20+40` (40 lines), `:-30` (last 30), `:5-10,20-30`; append `:raw` for verbatim text without prefixes (`:20:raw` is one line; URL selectors go before `?`/`#`). ",
    "Use `next_path` to continue paginated reads. Skills return in full by default (`/<path>` for resources, `/.` for root listing). ",
    "PNG/JPEG/WebP/GIF (first frame) images are sent visually (<=20 MiB, <=32M px, no selectors). ",
    "Artifacts/pipes/URLs/skills/raw have no edit snapshot. ",
);
const AGENT_READ_DESC: &str = "Use `agent://<id>` (or `agent://<id>/<run_id>`) for subagent results (paginate with `next_path`), `history://[<id>]` for event logs, and `proc://<run_id>` for run status. ";
const HASHLINE_READ_SUFFIX: &str = "Editable local non-raw UTF-8 text starts with `[path#TAG]` and `N:text` lines; copy header into edit and re-read after every edit.";
const REPLACE_READ_SUFFIX: &str =
    "Local text preserves source newlines verbatim for edit `oldText`.";
const HASHLINE_EDIT_DESC: &str = concat!(
    r#"Atomically edit one UTF-8 file (no symlinks). Pass `input`: `[path#TAG]` from read/rg (optional `*** Begin Patch`/`*** End Patch`) with `PUT N.=M:` (replace), `PUT <N:` / `PUT >N:` (insert), or `CUT N.=M` (delete). "#,
    r#"Prefix every PUT body row with `+` (blank row `+`). Targets original snapshot; no overlaps/no-ops; only displayed lines editable. Example: {"input":"[demo.txt#A1B2]\nPUT 1.=1:\n+edited"}"#,
);
const REPLACE_EDIT_DESC: &str = concat!(
    r#"Atomically edit an existing UTF-8 file (no symlinks) using exact non-overlapping `oldText` -> `newText` replacements matched against last read. "#,
    r#"Each `oldText` must be unique (include surrounding context). Example: {"path":"demo.txt","edits":[{"oldText":"old","newText":"new"}]}"#,
);
const WRITE_DESC: &str = "Atomically create or overwrite a UTF-8 file (creating parent dirs; no symlinks). Prefer `edit` for partial changes.";
const AGENT_WRITE_SUFFIX: &str = " Also accepts `agent://<id>` (`content` = follow-up message) and `proc://<run_id>/kill` (cancels a running subagent).";
const BASH_TASK_DESC: &str = concat!(
    "Manage persistent background services started by `bash(mode=\"background\")` only (never finite builds/tests). ",
    "`status` inspects cumulative output, `artifact://` URI, and `pipe://` metadata; `stop` terminates and cleans up idempotently. Verify readiness with real requests before stopping.",
);

pub(super) fn sandbox_net_note(status: &pp_sandbox::SandboxStatus) -> &'static str {
    if status.network {
        "network access is enabled"
    } else if status.backend == "bubblewrap" {
        "network access is isolated (no outbound or host network access)"
    } else {
        "outbound network is blocked (loopback allowed)"
    }
}

fn sandbox_exec_note(status: &pp_sandbox::SandboxStatus) -> String {
    match status.effective {
        pp_sandbox::EffectiveMode::Off => "No filesystem sandbox.".into(),
        pp_sandbox::EffectiveMode::WorkspaceWrite => format!(
            "Sandboxed (workspace-write; writes restricted to workspace/tmp, protected dirs .git/.agents/.pp read-only, credential paths unreadable, {}).",
            sandbox_net_note(status)
        ),
        pp_sandbox::EffectiveMode::ReadOnly => format!(
            "Sandboxed (read-only; workspace is read-only, credential paths unreadable, {}).",
            sandbox_net_note(status)
        ),
    }
}

fn tool_def(name: &str, description: impl Into<String>, parameters: Value) -> FunctionObject {
    FunctionObject {
        name: name.into(),
        description: Some(description.into()),
        parameters: Some(parameters),
        ..Default::default()
    }
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn read_definition(
    edit_mode: EditMode,
    limits: ToolLimits,
    include_agent_uri: bool,
    sandbox_active: bool,
) -> FunctionObject {
    let suffix = match edit_mode {
        EditMode::Hashline => HASHLINE_READ_SUFFIX,
        EditMode::Replace => REPLACE_READ_SUFFIX,
    };
    let agent_desc = if include_agent_uri {
        AGENT_READ_DESC
    } else {
        ""
    };
    let sandbox_note = if sandbox_active {
        " Protected credential paths are unreadable by sandbox policy."
    } else {
        ""
    };
    let limits_note = format!(
        "Default page cap: {} bytes / {} lines. ",
        limits.read_max_bytes, limits.read_max_lines
    );
    tool_def(
        "read",
        format!("{COMMON_READ_DESC}{limits_note}{agent_desc}{suffix}{sandbox_note}"),
        object_schema(json!({"path": {"type": "string"}}), &["path"]),
    )
}

fn edit_definition(edit_mode: EditMode) -> FunctionObject {
    match edit_mode {
        EditMode::Hashline => tool_def(
            "edit",
            HASHLINE_EDIT_DESC,
            object_schema(json!({"input": {"type": "string"}}), &["input"]),
        ),
        EditMode::Replace => tool_def(
            "edit",
            REPLACE_EDIT_DESC,
            object_schema(
                json!({
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {"type": "string", "minLength": 1},
                                "newText": {"type": "string"}
                            },
                            "required": ["oldText", "newText"],
                            "additionalProperties": false
                        }
                    }
                }),
                &["path", "edits"],
            ),
        ),
    }
}

fn write_definition(include_agent_uri: bool) -> FunctionObject {
    let desc = if include_agent_uri {
        format!("{WRITE_DESC}{AGENT_WRITE_SUFFIX}")
    } else {
        WRITE_DESC.into()
    };
    tool_def(
        "write",
        desc,
        object_schema(
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            &["path", "content"],
        ),
    )
}

fn bash_definition(
    limits: ToolLimits,
    sandbox: &pp_sandbox::SandboxStatus,
    include_sandbox_permissions: bool,
) -> FunctionObject {
    let default_timeout = limits.bash_timeout_seconds;
    let max_timeout = limits.bash_max_timeout_seconds;
    let sandbox_note = sandbox_exec_note(sandbox);
    let description = format!(
        concat!(
            "Run an embedded Bash-compatible shell in cwd (no rc/profiles/job control or independent PID: `$$` fails, `$!` works for simple external jobs, `exec` ends the logical shell, `ulimit` constrains external children only). ",
            "Finite commands (tests, builds, checks) MUST use foreground mode (default timeout {default_timeout}s; allowed 1-{max_timeout}s; 0 invalid). ",
            "Use `mode: \"background\"` ONLY for persistent services (returns `task_id`, managed via `bash_task`). ",
            "{sandbox_note} ",
            "Merged stdout/stderr returns bounded preview + `artifact://<id>` overflow + `pipe://` stage mirrors; run tests/builds bare without piping to head/tail/grep."
        ),
        default_timeout = default_timeout,
        max_timeout = max_timeout,
        sandbox_note = sandbox_note,
    );
    let timeout_desc = format!(
        "Foreground timeout in seconds (1-{max_timeout}, default {default_timeout}; ignored in background)."
    );
    let mut properties = json!({
        "command": {"type": "string"},
        "mode": {"type": "string", "enum": ["foreground", "background"]},
        "timeout_seconds": {
            "type": "integer",
            "minimum": 1,
            "maximum": max_timeout,
            "description": timeout_desc
        }
    });
    if include_sandbox_permissions {
        properties["sandbox_permissions"] = json!({
            "type": "object",
            "additionalProperties": false,
            "description": "Request permission before starting this command when it requires additional writable directories or outbound network. Normal commands omit this field. A sandbox failure does not retry the command.",
            "properties": {
                "writable": {"type": "array", "items": {"type": "string", "description": "An existing absolute directory path"}},
                "network": {"type": "boolean", "description": "Allow outbound network for the command and its subprocesses"}
            }
        });
    }
    tool_def("bash", description, object_schema(properties, &["command"]))
}

fn bash_task_definition() -> FunctionObject {
    tool_def(
        "bash_task",
        BASH_TASK_DESC,
        object_schema(
            json!({
                "task_id": {"type": "string", "minLength": 1},
                "action": {"type": "string", "enum": ["status", "stop"]}
            }),
            &["task_id", "action"],
        ),
    )
}

fn eval_definition(limits: ToolLimits, sandbox: &pp_sandbox::SandboxStatus) -> FunctionObject {
    let default_timeout = limits.eval_timeout_seconds;
    let max_timeout = limits.eval_max_timeout_seconds;
    let sandbox_note = sandbox_exec_note(sandbox);
    let description = format!(
        concat!(
            "Run one Python cell in a persistent Python 3.9+ kernel (state persists until `reset: true` or `/new`). ",
            "Supports top-level `await`, trailing expression echo, `%load`/`%pip`/`%cd`/`%pwd`/`%ls`/`%env`/`%time`/`%who`, `!cmd`, `%%bash`, matplotlib/PIL images (max 4/cell), ",
            "and helpers `tool.<name>(...)`, `read`, `write`, `rg`, `glob`, `sh`, `env` (nested `tool.*` pauses compute timeout, follows normal permissions, and cannot call `eval`/`yield`/`plan_*`). ",
            "{sandbox_note} Default timeout {default_timeout}s (1-{max_timeout}s)."
        ),
        default_timeout = default_timeout,
        max_timeout = max_timeout,
        sandbox_note = sandbox_note,
    );
    let timeout_desc =
        format!("Active compute timeout in seconds (1-{max_timeout}, default {default_timeout}).");
    tool_def(
        "eval",
        description,
        object_schema(
            json!({
                "language": {"type": "string", "enum": ["py"]},
                "code": {"type": "string"},
                "title": {"type": "string", "description": "Optional short label for this cell."},
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": max_timeout,
                    "description": timeout_desc
                },
                "reset": {"type": "boolean", "description": "Reset the kernel state before running this cell."}
            }),
            &["language", "code"],
        ),
    )
}

pub(super) struct ExecuteCall<'a, 'f> {
    pub(super) name: &'a str,
    pub(super) arguments: &'a str,
    pub(super) call_id: &'a str,
    pub(super) policy: ToolPolicy<'a>,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) emit: &'a mut dyn FnMut(AgentEvent),
    pub(super) web: &'a mut web_search::WebHandler<'f>,
    pub(super) nested_hook_context: Vec<String>,
    pub(super) nested_hook_halt: Option<HookHalt>,
}

impl Tools {
    pub fn all_definitions(&self, mode: WorkMode) -> Vec<FunctionObject> {
        let mut definitions = self.mode_definitions(mode);
        let access = self.mcp_access();
        if self.uses_mcp_proxy(access) {
            definitions.push(mcp_proxy::search_definition());
            definitions.push(mcp_proxy::call_definition());
        } else {
            definitions.extend(self.mcp.definitions(access));
        }
        if let Some(binding) = &self.subagent {
            definitions.extend(binding.definitions(mode));
            if mode == WorkMode::Plan {
                definitions.push(wait::definition(WorkMode::Plan));
            }
        }
        if self.yield_schema.is_some() {
            definitions.push(yield_definition());
        }
        if let Some(allowed) = &self.allowed_tools {
            definitions.retain(|d| allowed.allows(&d.name) || d.name == "yield");
        }
        definitions
    }

    fn mode_definitions(&self, mode: WorkMode) -> Vec<FunctionObject> {
        let include_agent_uri = self
            .allowed_tools
            .as_ref()
            .is_none_or(crate::AllowedTools::enables_subagents);
        let (permissions, sandbox_mode) = self.permission.snapshot_with_sandbox();
        let sandbox_status = self.sandbox.view(sandbox_mode).status();
        let sandbox_active = sandbox_status.effective != pp_sandbox::EffectiveMode::Off;
        let include_sandbox_permissions =
            sandbox_active && permissions != pp_config::config::PermissionMode::DangerFullAccess;
        let mut base = Self::definitions_for_context(
            self.edit_mode,
            self.limits,
            include_agent_uri,
            &sandbox_status,
            include_sandbox_permissions,
        );
        if self.eval_available && mode == WorkMode::Execute {
            base.push(eval_definition(self.limits, &sandbox_status));
        }
        let mut definitions = match mode {
            WorkMode::Execute => base,
            // Plan 和 Review 共用只读搜索工具。
            WorkMode::Plan => base
                .into_iter()
                .filter(|definition| {
                    PLAN_BASE_TOOLS.contains(&definition.name.as_str())
                        || definition.name == "write"
                })
                .collect(),
        };
        if self.ask_handler.is_some() {
            definitions.push(pp_core::interaction::definition());
        }
        definitions.push(glob::definition(sandbox_active));
        definitions.push(rg::definition(self.edit_mode, sandbox_active));
        if self.web_search.enabled {
            definitions.push(web_search::definition(&sandbox_status));
        }
        let lsp_readonly = match mode {
            WorkMode::Execute => self.lsp_readonly,
            WorkMode::Plan => true,
        };
        if mode == WorkMode::Plan {
            if let Some(write) = definitions.iter_mut().find(|tool| tool.name == "write") {
                write.description = Some(PLAN_WRITE_DESC.into());
            }
            definitions.extend(Self::plan_definitions());
        }
        if let Some(definition) = self.lsp_definition(lsp_readonly) {
            definitions.push(definition);
        }
        definitions
    }

    /// Read-only definitions for the independent plan reviewer: a subset of
    /// the Plan list without ask or the plan-writing tools.
    pub fn review_definitions(&self) -> Vec<FunctionObject> {
        let mut definitions: Vec<FunctionObject> = self
            .mode_definitions(WorkMode::Plan)
            .into_iter()
            .filter(|definition| {
                let name = definition.name.as_str();
                PLAN_BASE_TOOLS.contains(&name)
                    || PLAN_SEARCH_TOOLS.contains(&name)
                    || name == "lsp"
            })
            .collect();
        if self.uses_mcp_proxy(pp_mcp::Access::ReadOnly) {
            definitions.push(mcp_proxy::search_definition());
            definitions.push(mcp_proxy::call_definition());
        } else {
            definitions.extend(self.mcp.definitions(pp_mcp::Access::ReadOnly));
        }
        if let Some(allowed) = &self.allowed_tools {
            definitions.retain(|d| allowed.allows(&d.name));
        }
        definitions
    }

    fn plan_definitions() -> Vec<FunctionObject> {
        vec![tool_def(
            "plan_submit",
            PLAN_SUBMIT_DESC,
            object_schema(
                json!({"expected_revision": {"type": "integer", "minimum": 1}}),
                &["expected_revision"],
            ),
        )]
    }

    pub fn definitions(edit_mode: EditMode, limits: ToolLimits) -> Vec<FunctionObject> {
        let sandbox = pp_sandbox::Sandbox::disabled().status();
        Self::definitions_for_context(edit_mode, limits, true, &sandbox, false)
    }

    fn definitions_for_context(
        edit_mode: EditMode,
        limits: ToolLimits,
        include_agent_uri: bool,
        sandbox: &pp_sandbox::SandboxStatus,
        include_sandbox_permissions: bool,
    ) -> Vec<FunctionObject> {
        let sandbox_active = sandbox.effective != pp_sandbox::EffectiveMode::Off;
        vec![
            read_definition(edit_mode, limits, include_agent_uri, sandbox_active),
            edit_definition(edit_mode),
            write_definition(include_agent_uri),
            bash_definition(limits, sandbox, include_sandbox_permissions),
            bash_task_definition(),
            todo::definition(),
            wait::definition(WorkMode::Execute),
        ]
    }

    pub fn execute(
        &self,
        name: &str,
        arguments: &str,
        call_id: &str,
        policy: ToolPolicy<'_>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> ToolExecution {
        self.execute_with_fetch(
            name,
            arguments,
            call_id,
            policy,
            None,
            cancellation,
            emit,
            &mut |_, _| anyhow::bail!("HTTP fetch handler unavailable"),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_with_fetch(
        &self,
        name: &str,
        arguments: &str,
        call_id: &str,
        policy: ToolPolicy<'_>,
        hook: Option<&dyn ToolHookCall>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
        fetch: &mut read::Fetch<'_>,
    ) -> ToolExecution {
        self.execute_with_web(
            name,
            arguments,
            call_id,
            policy,
            hook,
            cancellation,
            emit,
            &mut |action, token| match action {
                web_search::WebAction::Read(url) => {
                    fetch(url, token).map(web_search::WebOutcome::Read)
                }
                web_search::WebAction::Search { .. } => {
                    anyhow::bail!("web search handler unavailable")
                }
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_with_web(
        &self,
        name: &str,
        arguments: &str,
        call_id: &str,
        policy: ToolPolicy<'_>,
        hook: Option<&dyn ToolHookCall>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
        web: &mut web_search::WebHandler<'_>,
    ) -> ToolExecution {
        let mut call = ExecuteCall {
            name,
            arguments,
            call_id,
            policy,
            cancellation,
            emit,
            web,
            nested_hook_context: Vec::new(),
            nested_hook_halt: None,
        };
        let started = Instant::now();
        let pid = std::process::id();
        let tool = tool_metric_name(call.name);
        tracing::info!(event = "tool_started", pid, tool);
        (call.emit)(AgentEvent::ToolStarted {
            id: call.call_id.to_owned(),
            name: call.name.to_owned(),
            arguments: call.arguments.to_owned(),
        });
        // The cancellation check runs after ToolStarted and before any argument
        // parsing, file access or process creation.
        let mut images = Vec::new();
        let (mut value, stop_error, plan_change, todo_change) =
            self.run_checked(&mut call, hook, &mut images);
        let (mut hook_context, mut hook_halt, serialized) = finalize_tool_execution(
            &mut call,
            hook,
            pid,
            tool,
            started,
            &mut value,
            stop_error.is_some(),
            &self.artifacts,
            self.limits.output,
        );
        if !call.nested_hook_context.is_empty() {
            hook_context.extend(call.nested_hook_context);
        }
        if hook_halt.is_none() {
            hook_halt = call.nested_hook_halt;
        }
        ToolExecution {
            output: serialized,
            images,
            stop_error,
            plan_change,
            todo_change,
            hook_context,
            hook_halt,
        }
    }

    fn run_checked(
        &self,
        call: &mut ExecuteCall<'_, '_>,
        hook: Option<&dyn ToolHookCall>,
        images: &mut Vec<read::ToolImage>,
    ) -> (
        Value,
        Option<anyhow::Error>,
        Option<PlanChange>,
        Option<todo::TodoChange>,
    ) {
        // hook deny 在进入审批门与参数解析之前生效，YOLO 也不能绕过。
        let denied_by_hook = hook.and_then(|hook| hook.deny_reason());
        let hook_ask = hook.and_then(|hook| hook.ask_reason());
        if call.cancellation.is_cancelled() {
            return (
                json!({"ok":false,"cancelled":true,"error":"Not executed: task cancelled."}),
                None,
                None,
                None,
            );
        }
        if let Some(reason) = denied_by_hook {
            return (
                json!({"ok":false,"error": format!("Not executed: {reason}")}),
                None,
                None,
                None,
            );
        }
        match self.execute_inner(call, hook, hook_ask, images) {
            Ok((result, change, todo)) => {
                if call.name == "task" {
                    self.reserve_task_results(&result);
                }
                (result, None, change, todo)
            }
            Err(error) => {
                let cleanup_failure = error.chain().any(|cause| cause.is::<CleanupError>());
                let cancelled = error.chain().any(|cause| cause.is::<Cancelled>());
                let message = format!("{error:#}");
                if cleanup_failure {
                    // A tool that could not be stopped is a real failure;
                    // keep the typed error so the agent can report it.
                    (json!({"ok":false,"error":message}), Some(error), None, None)
                } else if cancelled {
                    (
                        json!({"ok":false,"cancelled":true,"error":"Tool cancelled; side effects may have occurred."}),
                        None,
                        None,
                        None,
                    )
                } else {
                    (json!({"ok":false,"error":message}), None, None, None)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
