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

/// Summarize observed non-zero pipeline stages from the tool result.
pub fn pipeline_failure_notice(value: &Value) -> Option<String> {
    let count = value["pipeline_failures"]["count"]
        .as_u64()
        .or_else(|| value["pipes"]["nonzero_stages"].as_u64())?;
    (count > 0).then(|| format!("Non-zero pipeline stages: {count}"))
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
