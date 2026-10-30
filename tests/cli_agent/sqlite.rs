//! 按真实逻辑模型请求累计使用次数的端到端回归。
//!
//! 口径：实际尝试发送的逻辑模型 API 请求 +1，与 stats 写入无关，
//! 建连重试不额外计数，发送前失败（授权校验）不计数，跨重启保留。

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
    // 迁移已完成；default_model 已由 agent.db 权威，settings.yml 不再接受旧字段。
    "{\"rate_limit_retry_budget_seconds\":30}\n".to_string()
}

#[test]
fn sqlite_model_usage_counts_logical_requests() {
    for protocol in ["completion", "response"] {
        let responses = protocol == "response";
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if responses {
            response_models(&server, Some("response"))
        } else {
            fixture_models(&server)
        };
        // 第一轮：工具续轮 + 最终答复 => 2 次逻辑请求。
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

        // 第二轮重启：429 一次后成功 => 逻辑请求恰好 +1（HTTP 请求两次），计数跨进程保留。
        std::fs::write(config_path(dir.path(), "settings.yml"), retry_settings()).unwrap();
        let cli = Cli::start_with_existing_files(dir.path(), Some("retry run"));
        let (first, first_body) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        first
            .respond(
                Response::from_string("private-error-body fixture-key")
                    .with_status_code(429)
                    .with_header(Header::from_bytes(&b"Retry-After"[..], &b"0"[..]).unwrap()),
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

        // 第三轮重启：发送后的永久失败也计一次；进程失败不丢已计数。
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
            .respond(Response::from_string("private-error-body fixture-key").with_status_code(401))
            .unwrap();
        let output = cli.finish();
        assert!(!output.status.success(), "{protocol}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains("private-error-body") && !error.contains("fixture-key"));
        assert_eq!(usage(dir.path(), "fixture/primary"), Some(4), "{protocol}");
        no_more(&server);
        // stats/log GC 不承担使用次数权威，清理后累计值必须保持。
        let output = Cli::spawn(dir.path(), &["gc", "1"], &[]).finish();
        assert!(output.status.success(), "{protocol}");
        assert_eq!(usage(dir.path(), "fixture/primary"), Some(4), "{protocol}");
    }

    // stats 不可用时任务照常完成且计数照常累加。
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

    // 授权校验失败发生在发送前：不计数。
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
