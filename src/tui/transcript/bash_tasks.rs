//! 后台进程的启动、查询和停止共用一张可更新卡片，原始工具事件保持不变。

use super::*;

impl Transcript {
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
