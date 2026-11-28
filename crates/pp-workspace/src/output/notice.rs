//! Display text for output status and recovery guidance (no file access).
use serde_json::{Value, json};

/// Follow-up read guidance embedded in tool results; structured metadata is kept
/// separate so terminal display text never leaks into log bodies.
pub(super) fn recovery_notice(value: &Value) -> Option<String> {
    if value["capture_error"].is_string() {
        return Some("Artifact capture failed; full saved output is unavailable. No artifact is available for recovery.".into());
    }
    let artifact = &value["artifact"];
    let uri = artifact["uri"].as_str()?;
    let capturing = artifact["capturing"] == true;
    let elided = artifact["elided_bytes"].as_u64().unwrap_or(0);
    let mut status = if capturing {
        "Capturing snapshot (still growing).".to_owned()
    } else if artifact["complete"] == true {
        "Full saved output available.".to_owned()
    } else {
        "Saved output is incomplete.".to_owned()
    };
    if elided > 0 {
        status.push_str(&format!(
            " (head/tail sample, {elided} middle bytes omitted and cannot be recovered.)"
        ));
    }
    let read = json!({"path":format!("{uri}:-80")});
    let rg = json!({"path":uri,"pattern":"REGEX"});
    Some(format!(
        "{status} Use read({read}) or rg({rg}); artifact:// is a tool URI, not a shell path."
    ))
}

/// Bounded stage summary of `result.pipes`; metadata only, no registry access and no stage bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipesStatus {
    pub run_id: String,
    pub uri: String,
    pub nonzero_stages: u64,
    pub incomplete: bool,
}

impl PipesStatus {
    /// CLI trailer line: URI, non-zero stage count and the incomplete observation/retention hint.
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

/// Extract a bounded summary from `pipes` metadata; returns `None` when there is no real pipeline and older results stay unchanged.
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

/// Build display text from saved metadata only; the UI never touches files here.
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
        // A run without real pipelines does not use the card footer entry.
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
