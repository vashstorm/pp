//! 后台进程的启动、查询和停止共用一张可更新卡片，原始工具事件保持不变。

use super::*;

impl Transcript {
    pub(super) fn apply_wait_background(&mut self, result: &Value) {
        for snapshot in result["background_results"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(index) = snapshot["task_id"]
                .as_str()
                .and_then(|id| self.background_bash.get(id))
                .copied()
            else {
                continue;
            };
            let entry = &mut self.entries[index];
            entry.status = if snapshot["ok"] == false {
                EntryStatus::Failed
            } else {
                EntryStatus::Complete
            };
            entry.text = tool_result(EntryKind::Bash, "bash", snapshot);
            entry.shell = snapshot
                .get("shell")
                .and_then(|value| {
                    serde_json::from_value::<pp_core::shell_display::ShellPresentation>(
                        value.clone(),
                    )
                    .ok()
                })
                .filter(|shell| shell.version == pp_core::shell_display::VERSION);
            entry.result = Some(snapshot.clone());
            entry.stdout.clear();
            entry.stderr.clear();
            entry.sync_pipes_badge(false);
            entry.update_bash_footer(
                entry
                    .started
                    .filter(|_| !self.replaying)
                    .map(|s| s.elapsed()),
            );
            entry.layout = None;
        }
    }

    pub(super) fn background_bash_index(&self, arguments: &Value) -> Option<usize> {
        if !matches!(arguments["action"].as_str(), Some("status" | "stop")) {
            return None;
        }
        self.background_bash
            .get(arguments["task_id"].as_str()?)
            .copied()
    }

    pub(super) fn merge_background_bash_result(&mut self, index: usize, result: &mut Value) {
        let entry = &self.entries[index];
        let background = match entry.tool.as_deref() {
            Some("bash") => entry.arguments["mode"] == "background",
            Some("bash_task") => {
                matches!(entry.arguments["action"].as_str(), Some("status" | "stop"))
            }
            _ => false,
        };
        if !background {
            return;
        }
        if let Some(task_id) = result["task_id"].as_str() {
            self.background_bash.insert(task_id.to_owned(), index);
        } else if self
            .background_bash
            .values()
            .any(|existing| *existing == index)
            && let (Some(previous), Some(error)) = (
                entry.result.as_ref().and_then(Value::as_object),
                result.as_object(),
            )
        {
            // 查询失败也更新原框，保留此前累计输出和最后已知进程状态。
            let mut merged = previous.clone();
            merged.extend(error.clone());
            *result = Value::Object(merged);
        }
    }
}
