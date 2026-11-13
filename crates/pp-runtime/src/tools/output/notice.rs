//! 输出状态与恢复指引的展示文案（不访问文件）。
use serde_json::{Value, json};

/// 模型结果中的补读指引；保留结构化元数据，不把终端展示文案混入日志正文。
pub(super) fn recovery_notice(value: &Value) -> Option<String> {
    if value["capture_error"].is_string() {
        return Some("Artifact capture failed; full saved output is unavailable. No artifact is available for recovery.".into());
    }
    let artifact = &value["artifact"];
    let uri = artifact["uri"].as_str()?;
    let capturing = artifact["capturing"] == true;
    let elided = artifact["elided_bytes"].as_u64().unwrap_or(0);
    let mut status = if capturing {
        "Saved output is still growing; each read/rg uses one snapshot.".to_owned()
    } else if artifact["complete"] == true {
        "Full saved output is available.".to_owned()
    } else {
        "Saved output is incomplete.".to_owned()
    };
    if elided > 0 {
        status.push_str(&format!(
            " The artifact is a head/tail sample with {elided} bytes omitted; missing middle bytes cannot be recovered from it."
        ));
    }
    let read = json!({"path":format!("{uri}:-80")});
    let rg = json!({"path":uri,"pattern":"REGEX"});
    Some(format!(
        "{status} Call the built-in read tool as read({read}) for the last 80 saved lines, or the built-in rg tool as rg({rg}) for targeted searches (replace REGEX with your pattern). artifact:// URIs are tool resources, not filesystem paths. Only the built-in rg tool supports searching them; NEVER pass them to bash or shell commands such as grep, rg, cat, sed, or awk. Use read line selectors or next_path to inspect other saved ranges."
    ))
}

/// `result.pipes` 的有界阶段摘要；只读元数据，不访问 registry、没有阶段正文。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipesStatus {
    pub(crate) run_id: String,
    pub uri: String,
    pub nonzero_stages: u64,
    pub incomplete: bool,
}

impl PipesStatus {
    /// CLI 追加行：URI、非零阶段计数与观测/保留不完整提示。
    pub fn notice(&self) -> String {
        let mut status = format!("[pipes: {}", self.uri);
        if self.nonzero_stages > 0 {
            status.push_str(&format!(
                " · Non-zero stage results: {}",
                self.nonzero_stages
            ));
        }
        if self.incomplete {
            status.push_str(" · stage output incomplete");
        }
        status.push(']');
        status
    }
}

/// 从 `pipes` 元数据提取有界摘要；没有真实管道时返回 `None`，老结果保持原样。
pub fn pipes_status(pipes: &Value) -> Option<PipesStatus> {
    if !pipes.is_object() {
        return None;
    }
    let run_id = pipes["run_id"].as_str()?;
    let pipelines = pipes["pipelines"].as_array().map_or(0, Vec::len);
    if pipelines == 0 && pipes["omitted_pipelines"].as_u64().unwrap_or(0) == 0 {
        return None;
    }
    Some(PipesStatus {
        run_id: run_id.to_owned(),
        uri: pipes["uri"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("pipe://{run_id}")),
        nonzero_stages: pipes["nonzero_stages"].as_u64().unwrap_or(0),
        incomplete: pipes["incomplete"] == true || pipes["capture_error"].is_string(),
    })
}

/// 仅从已保存元数据生成展示文案，不在 UI 访问文件。
pub fn notice(value: &Value) -> String {
    let mut notices = Vec::new();
    if value["truncated"] == true {
        notices.push(
            if value["artifact"]["uri"].is_string() {
                "[Output truncated; inspect the artifact for more.]"
            } else {
                "[Output truncated; full saved output is unavailable.]"
            }
            .to_owned(),
        );
    }
    if let Some(uri) = value["artifact"]["uri"].as_str() {
        let state = if value["artifact"]["capturing"] == true {
            "capturing; reads use a snapshot"
        } else if value["artifact"]["complete"] == true {
            "complete"
        } else {
            "head/tail sample"
        };
        notices.push(format!(
            "[{uri}: {state}; {} bytes omitted from artifact]",
            value["artifact"]["elided_bytes"].as_u64().unwrap_or(0)
        ));
    }
    if let Some(error) = value["capture_error"].as_str() {
        notices.push(format!("[{error}]"));
    }
    notices.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipes_status_is_bounded_and_absent_for_old_results() {
        assert!(pipes_status(&Value::Null).is_none());
        assert!(pipes_status(&json!({"ok":true,"stdout":"plain"})).is_none());
        // 没有真实管道的 run 不占用卡片底部入口。
        assert!(pipes_status(&json!({"run_id":"abc","uri":"pipe://abc"})).is_none());

        let status = pipes_status(&json!({
            "run_id":"abc",
            "uri":"pipe://abc",
            "pipelines":[{"id":1,"stages":[]}],
            "streams":[{"id":0,"uri":"pipe://abc/0"}],
            "nonzero_stages":2,
            "incomplete":true
        }))
        .unwrap();
        assert_eq!(status.uri, "pipe://abc");
        assert_eq!(status.nonzero_stages, 2);
        assert!(status.incomplete);
        let notice = status.notice();
        assert!(notice.contains("pipe://abc"), "{notice}");
        assert!(notice.contains("Non-zero stage results: 2"), "{notice}");
        assert!(notice.contains("stage output incomplete"), "{notice}");
        assert!(!notice.contains("body-secret"), "{notice}");

        let omitted = pipes_status(&json!({
            "run_id":"def",
            "pipelines":[],
            "omitted_pipelines":3
        }))
        .unwrap();
        assert_eq!(omitted.uri, "pipe://def");
        assert!(!omitted.notice().contains("Non-zero"));
    }
}
