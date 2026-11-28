use super::*;

pub(super) fn assert_level(body: &Value, protocol: &str, level: &str) {
    let field = if protocol == "response" {
        "reasoning"
    } else {
        "reasoning_effort"
    };
    let other = if protocol == "response" {
        "reasoning_effort"
    } else {
        "reasoning"
    };
    assert!(body.get(other).is_none());
    let level = if level == "default" { "none" } else { level };
    if protocol == "response" {
        assert_eq!(body[field]["effort"], level);
        if level == "none" {
            assert!(body[field].get("summary").is_none());
        } else {
            assert_eq!(body[field]["summary"], "concise");
        }
    } else {
        assert_eq!(body[field], level);
    }
}

pub(super) fn finish_request(request: CapturedRequest, protocol: &str) {
    if protocol == "response" {
        response_final(request, "ok");
    } else {
        final_reply(request, "ok");
    }
}

#[test]
fn think_level_settings_reach_both_apis_including_none_and_default_alias() {
    for protocol in ["completion", "response"] {
        for level in ["none", "default", "low", "medium", "high", "xhigh", "max"] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let settings = json!({"think_level":level}).to_string();
            let models = response_models(&server, Some(protocol));
            let cli = Cli::start_with(dir.path(), Some("test"), &settings, &models);
            let path = if protocol == "response" {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            let (request, body) = receive_at(&server, path);
            assert_level(&body, protocol, level);
            finish_request(request, protocol);
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"ok\n");
        }
    }
}
