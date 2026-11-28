//! Value types for lifecycle command hooks: the user-level `~/.pp/agent/hooks.yml` and
//! the project-level `<startup directory>/.pp/hooks.yml` share one shape.
//!
//! Only the configuration shape and structural validation live here; regex compilation, event matching and process execution are in runtime.
//! Strict parsing: unknown event names and unknown handler fields are always rejected instead of being
//! silently ignored for Claude configuration compatibility.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Maximum hook timeout in seconds; anything larger is rejected as a configuration error.
pub const MAX_HOOK_TIMEOUT_SECONDS: u64 = 3600;
/// Default `timeout`: 30 seconds.
pub const DEFAULT_HOOK_TIMEOUT_SECONDS: u64 = 30;

/// The five supported lifecycle events; the wire name is the enum name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HookEvent {
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Stop,
}

impl HookEvent {
    /// Fixed order of all events; runtime uses it to build its index table.
    pub const ALL: [Self; 5] = [
        Self::UserPromptSubmit,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PostToolUseFailure,
        Self::Stop,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::Stop => "Stop",
        }
    }

    /// Tool events match pp native tool names; other events never match a tool name.
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::PreToolUse | Self::PostToolUse | Self::PostToolUseFailure
        )
    }

    /// Control points: internal failures are fail-closed by default; post events allow by default and warn.
    pub fn is_control_point(self) -> bool {
        !matches!(self, Self::PostToolUse | Self::PostToolUseFailure)
    }
}

/// The only supported handler type: an external command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookHandlerType {
    Command,
}

/// Explicit policy for handler-internal failures; when omitted, the event's default policy applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookFailurePolicy {
    Block,
    Continue,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandHook {
    #[serde(rename = "type")]
    pub kind: HookHandlerType,
    pub command: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(rename = "onFailure", default)]
    pub on_failure: Option<HookFailurePolicy>,
}

fn default_timeout() -> u64 {
    DEFAULT_HOOK_TIMEOUT_SECONDS
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookMatcherGroup {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<CommandHook>,
}

/// The five event arrays of `hooks:`; all are empty by default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksSettings {
    #[serde(rename = "UserPromptSubmit")]
    pub user_prompt_submit: Vec<HookMatcherGroup>,
    #[serde(rename = "PreToolUse")]
    pub pre_tool_use: Vec<HookMatcherGroup>,
    #[serde(rename = "PostToolUse")]
    pub post_tool_use: Vec<HookMatcherGroup>,
    #[serde(rename = "PostToolUseFailure")]
    pub post_tool_use_failure: Vec<HookMatcherGroup>,
    #[serde(rename = "Stop")]
    pub stop: Vec<HookMatcherGroup>,
}

impl HooksSettings {
    pub fn groups(&self, event: HookEvent) -> &[HookMatcherGroup] {
        match event {
            HookEvent::UserPromptSubmit => &self.user_prompt_submit,
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PostToolUse => &self.post_tool_use,
            HookEvent::PostToolUseFailure => &self.post_tool_use_failure,
            HookEvent::Stop => &self.stop,
        }
    }

    fn groups_mut(&mut self, event: HookEvent) -> &mut Vec<HookMatcherGroup> {
        match event {
            HookEvent::UserPromptSubmit => &mut self.user_prompt_submit,
            HookEvent::PreToolUse => &mut self.pre_tool_use,
            HookEvent::PostToolUse => &mut self.post_tool_use,
            HookEvent::PostToolUseFailure => &mut self.post_tool_use_failure,
            HookEvent::Stop => &mut self.stop,
        }
    }

    /// Appends groups from another source: project-level groups follow user-level ones, and matching within an event follows insertion order.
    pub fn extend(&mut self, other: &Self) {
        for event in HookEvent::ALL {
            self.groups_mut(event)
                .extend_from_slice(other.groups(event));
        }
    }

    pub fn is_empty(&self) -> bool {
        HookEvent::ALL
            .iter()
            .all(|event| self.groups(*event).is_empty())
    }

    /// Structural validation: commands, timeout ranges, and matchers on events that never match a tool name.
    /// Regex compilation is left to runtime, and error messages carry only the event name and index.
    pub fn validate(&self) -> Result<()> {
        for event in HookEvent::ALL {
            for (group_index, group) in self.groups(event).iter().enumerate() {
                let where_ = format!("hooks.{}[{}]", event.as_str(), group_index);
                if !event.is_tool_event()
                    && let Some(matcher) = &group.matcher
                {
                    ensure!(
                        matcher.is_empty() || matcher == "*",
                        "{where_}.matcher must be empty or \"*\" for {}",
                        event.as_str()
                    );
                }
                for (hook_index, hook) in group.hooks.iter().enumerate() {
                    ensure!(
                        !hook.command.trim().is_empty(),
                        "{where_}.hooks[{hook_index}].command must not be blank"
                    );
                    ensure!(
                        (1..=MAX_HOOK_TIMEOUT_SECONDS).contains(&hook.timeout),
                        "{where_}.hooks[{hook_index}].timeout must be between 1 and {MAX_HOOK_TIMEOUT_SECONDS}"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<HooksSettings> {
        Ok(serde_yaml::from_str(yaml)?)
    }

    #[test]
    fn hooks_parse_full_shape_with_defaults() {
        let settings = parse(
            "SessionStart: {future: true}\nPreToolUse:\n  - matcher: 'bash,edit'\n    extra: true\n    hooks:\n      - type: command\n        command: /bin/true\n        extra: true\nStop:\n  - hooks:\n      - type: command\n        command: /bin/true\n        args: ['--flag', 'a b']\n        timeout: 5\n        onFailure: block\n",
        )
        .unwrap();
        settings.validate().unwrap();
        let group = &settings.pre_tool_use[0];
        assert_eq!(group.matcher.as_deref(), Some("bash,edit"));
        assert_eq!(group.hooks[0].timeout, DEFAULT_HOOK_TIMEOUT_SECONDS);
        assert_eq!(group.hooks[0].on_failure, None);
        assert_eq!(group.hooks[0].args, None);
        let stop = &settings.stop[0].hooks[0];
        assert_eq!(
            stop.args.as_deref(),
            Some(&["--flag".to_owned(), "a b".to_owned()][..])
        );
        assert_eq!(stop.timeout, 5);
        assert_eq!(stop.on_failure, Some(HookFailurePolicy::Block));
        assert!(!settings.is_empty());
    }

    #[test]
    fn hooks_reject_unsupported_handler_types_and_failure_policies() {
        for yaml in [
            "PreToolUse:\n  - hooks:\n      - type: prompt\n        command: /bin/true\n",
            "PreToolUse:\n  - hooks:\n      - type: command\n        command: /bin/true\n        onFailure: retry\n",
        ] {
            assert!(parse(yaml).is_err(), "{yaml} must be rejected");
        }
    }

    #[test]
    fn hooks_reject_blank_command_timeout_and_tool_matcher_on_input_events() {
        let blank =
            parse("PreToolUse:\n  - hooks:\n      - type: command\n        command: '  '\n")
                .unwrap();
        assert!(blank.validate().is_err());
        let timeout = parse("PreToolUse:\n  - hooks:\n      - type: command\n        command: /bin/true\n        timeout: 0\n").unwrap();
        assert!(timeout.validate().is_err());
        let too_long = parse("PreToolUse:\n  - hooks:\n      - type: command\n        command: /bin/true\n        timeout: 3601\n").unwrap();
        assert!(too_long.validate().is_err());
        let matcher = parse("UserPromptSubmit:\n  - matcher: bash\n    hooks:\n      - type: command\n        command: /bin/true\n").unwrap();
        let error = matcher.validate().unwrap_err().to_string();
        assert!(error.contains("UserPromptSubmit"), "{error}");
        let tool_matcher =
            parse("PostToolUse:\n  - matcher: '^(bash|write)$'\n    hooks: []\n").unwrap();
        tool_matcher.validate().unwrap();
        assert!(parse("PostToolUse:\n  - hooks: {}\n").is_err());
    }
}
