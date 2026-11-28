//! End-to-end regression for accumulating usage counts on real logical model requests.
//!
//! Definition: a logical model API request that is actually attempted to be sent counts +1, regardless of stats
//! writes; connection retries do not add counts, failures before sending (authorization checks) do not count,
//! and the total survives restarts.

use super::*;
use rusqlite::Connection;

fn usage(home: &std::path::Path, reference: &str) -> Option<u64> {
    let database = Connection::open(home.join(".pp/agent/agent.db")).unwrap();
    database
        .query_row(
            "SELECT request_count FROM model_usage WHERE model_reference = ?1",
            [reference],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count as u64)
        .ok()
}

fn retry_settings() -> String {
    // Migration is already done; default_model is authoritative in agent.db and settings.yml no longer accepts the
    // old fields.
    "{\"rate_limit_retry_budget_seconds\":30}\n".to_string()
}

#[test]
fn sqlite_model_usage_counts_logical_requests() {
    {
        let protocol = "response";
        let responses = protocol == "response";
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if responses {
            response_models(&server, Some("response"))
        } else {
            fixture_models(&server)
        };
        // First round: tool continuation turn + final answer => 2 logical requests.
        let cli = Cli::start_with(
            dir.path(),
            Some("write then finish"),
            fixture_settings(),
            &models,
        );
        let (first, _) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        if responses {
            response_reply(
                first,
                json!([response_call(
                    "w1",
                    "write",
                    json!({"path":"out.txt","content":"hi"})
                )]),
            );
        } else {
            calls_reply(
                first,
                json!([call(
                    "w1",
                    "write",
                    json!({"path":"out.txt","content":"hi"})
                )]),
            );
        }
        let (second, _) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        if responses {
            response_final(second, "done");
        } else {
            final_reply(second, "done");
        }
        let output = cli.finish();
        assert!(output.status.success(), "{protocol}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "done\n");
        assert_eq!(usage(dir.path(), "fixture/primary"), Some(2), "{protocol}");
        no_more(&server);

        // Second restart: one 429 then success => exactly +1 logical request (two HTTP requests), and the count is
        // kept across processes.
        std::fs::write(config_path(dir.path(), "settings.yml"), retry_settings()).unwrap();
        let cli = Cli::start_with_existing_files(dir.path(), Some("retry run"));
        let (first, first_body) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        first
            .respond(
                Response::builder()
                    .status(429)
                    .header("Retry-After", "0")
                    .body(Body::from("private-error-body fixture-key"))
                    .unwrap(),
            )
            .unwrap();
        let (second, second_body) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        assert_eq!(first_body, second_body);
        if responses {
            response_final(second, "recovered");
        } else {
            final_reply(second, "recovered");
        }
        let output = cli.finish();
        assert!(output.status.success(), "{protocol}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "recovered\n");
        assert_eq!(usage(dir.path(), "fixture/primary"), Some(3), "{protocol}");
        no_more(&server);

        // Third restart: a permanent failure after sending also counts once; a failed process does not lose the
        // already counted usage.
        std::fs::write(
            config_path(dir.path(), "settings.yml"),
            "{\"rate_limit_retry_budget_seconds\":0}\n",
        )
        .unwrap();
        let cli = Cli::start_with_existing_files(dir.path(), Some("fail run"));
        let (request, _) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        request
            .respond(
                Response::builder()
                    .status(401)
                    .body(Body::from("private-error-body fixture-key"))
                    .unwrap(),
            )
            .unwrap();
        let output = cli.finish();
        assert!(!output.status.success(), "{protocol}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains("private-error-body") && !error.contains("fixture-key"));
        assert_eq!(usage(dir.path(), "fixture/primary"), Some(4), "{protocol}");
        no_more(&server);
        // stats/log GC is not the authority for usage counts, so the accumulated value must survive the cleanup.
        let output = Cli::spawn(dir.path(), &["gc", "1"], &[]).finish();
        assert!(output.status.success(), "{protocol}");
        assert_eq!(usage(dir.path(), "fixture/primary"), Some(4), "{protocol}");
    }

    // When stats is unavailable the task still completes and the count still accumulates.
    {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".pp")).unwrap();
        std::fs::write(dir.path().join(".pp/stats"), "blocked directory").unwrap();
        let cli = Cli::start(&server, dir.path(), Some("stats unavailable"));
        let (request, _) = receive(&server);
        final_reply(request, "task still completed");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"task still completed\n");
        assert_eq!(usage(dir.path(), "fixture/primary"), Some(1));
    }

    // An authorization check failure happens before sending: no count.
    {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = json!({"providers": {"fixture": {
            "auth": "glm",
            "models": {"primary": {"model": "fixture-model"}}
        }}})
        .to_string();
        let cli = Cli::start_with(
            dir.path(),
            Some("no credentials"),
            fixture_settings(),
            &models,
        );
        let output = cli.finish();
        assert!(!output.status.success());
        no_more(&server);
        assert_eq!(usage(dir.path(), "fixture/primary"), None);
    }
}
