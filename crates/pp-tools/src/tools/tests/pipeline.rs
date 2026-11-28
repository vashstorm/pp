//! Pipeline execution through the foreground and background Bash tools.

mod integration {
    use super::super::*;

    fn run_bash(tools: &Tools, command: &str) -> Value {
        execute(tools, "bash", json!({"command": command}))
    }

    fn failed_stages(result: &Value) -> Vec<(u64, u64, u64)> {
        let mut stages = result["pipeline_failures"]["stages"]
            .as_array()
            .expect("stage failures")
            .iter()
            .map(|stage| {
                (
                    stage["pipeline"].as_u64().unwrap(),
                    stage["stage"].as_u64().unwrap(),
                    stage["status"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        stages.sort_unstable();
        stages
    }

    #[test]
    fn pipeline_status_pipefail_sigpipe_and_stderr_edges() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

        for (command, exit, output, failures) in [
            (
                "{ printf 'hidden\nvisible\n'; exit 7; } | tail -n 1",
                0,
                "visible\n",
                vec![(1, 1, 7)],
            ),
            ("set -o pipefail; false | true", 1, "", vec![(1, 1, 1)]),
            ("! false | true", 1, "", vec![(1, 1, 1)]),
            (
                "false | false || printf recovered",
                0,
                "recovered",
                vec![(1, 1, 1), (1, 2, 1)],
            ),
        ] {
            let result = run_bash(&tools, command);
            assert_eq!(result["ok"], exit == 0, "{result}");
            assert_eq!(result["exit_code"], exit, "{result}");
            assert_eq!(result["output"], output, "{result}");
            assert_eq!(failed_stages(&result), failures, "{result}");
            assert_eq!(
                result["pipeline_failures"]["count"],
                failures.len(),
                "{result}"
            );
            assert!(result.get("pipes").is_none(), "{result}");
        }

        let result = run_bash(&tools, "yes x | head -n 1");
        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(result["output"], "x\n", "{result}");
        assert_eq!(failed_stages(&result), [(1, 1, 141)], "{result}");
        assert_eq!(
            result["pipeline_failures"]["stages"][0]["signal"], 13,
            "{result}"
        );

        for (command, expected) in [
            ("{ printf 'piped'; printf 'E' >&2; } | tr a-z A-Z", "PIPED"),
            ("{ printf 'o'; printf 'e' >&2; } 2>&1 | tr a-z A-Z", "OE"),
        ] {
            let result = run_bash(&tools, command);
            let output = result["output"].as_str().unwrap();
            assert!(output.contains(expected), "{result}");
            assert!(output.contains('E'), "{result}");
            assert!(result.get("pipeline_failures").is_none(), "{result}");
        }
        let bypass = run_bash(&tools, "printf 'bypassed' > bypass.txt | cat");
        assert_eq!(bypass["ok"], true, "{bypass}");
        assert_eq!(bypass["output"], "", "{bypass}");
        assert_eq!(
            fs::read_to_string(dir.path().join("bypass.txt")).unwrap(),
            "bypassed"
        );
    }

    #[test]
    fn pipeline_occurrences_cover_loops_functions_and_substitutions() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

        let result = run_bash(
            &tools,
            r#"for i in 1 2; do { printf "$i"; exit 7; } | cat; done
f() { { printf f; exit 8; } | cat; }
f
x=$({ printf s; exit 9; } | cat)
printf '%s' "$x"
({ printf a; exit 10; } | cat)
{ printf b; exit 11; } | cat & wait
"#,
        );
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["output"], "12fsab", "{result}");
        assert_eq!(
            failed_stages(&result),
            [
                (1, 1, 7),
                (2, 1, 7),
                (3, 1, 8),
                (4, 1, 9),
                (5, 1, 10),
                (6, 1, 11)
            ],
            "{result}"
        );
    }

    #[test]
    fn pipeline_metadata_stays_bounded_for_many_occurrences() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let result = run_bash(&tools, "for ((i=0;i<40;++i)); do false | true; done");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["pipeline_failures"]["count"], 40, "{result}");
        let stages = failed_stages(&result);
        assert_eq!(
            stages,
            (1..=32)
                .map(|pipeline| (pipeline, 1, 1))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn pipeline_transports_large_and_raw_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        for (command, expected) in [
            ("head -c 1200000 /dev/zero | cat | wc -c", "1200000"),
            (r"printf 'a\000b' | cat | wc -c", "3"),
            (
                r#"sh -c 'printf "\303"; sleep 0.05; printf "\251"' | cat"#,
                "é",
            ),
            (r"printf '\033[31mred\033[0m' | cat", "red"),
            (r"printf '\033]0;title\007body' | cat", "body"),
            ("printf 'first' | cat | tr a-z A-Z", "FIRST"),
        ] {
            let result = run_bash(&tools, command);
            assert_eq!(result["ok"], true, "{command}: {result}");
            assert_eq!(
                result["output"].as_str().unwrap().trim(),
                expected,
                "{command}: {result}"
            );
        }
    }

    #[test]
    fn pipeline_failure_finalization_and_not_found_once() {
        {
            struct Restore {
                root: std::path::PathBuf,
                moved: std::path::PathBuf,
            }

            impl Drop for Restore {
                fn drop(&mut self) {
                    let _ = fs::remove_file(&self.root);
                    let _ = fs::rename(&self.moved, &self.root);
                }
            }

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_owned();
            let moved = root.with_extension("moved");
            let tools = Tools::new(root.clone(), EditMode::Replace).unwrap();

            fs::rename(&root, &moved).unwrap();
            fs::write(&root, b"not a directory").unwrap();
            let _restore = Restore {
                root: root.clone(),
                moved,
            };

            let result = run_bash(&tools, "printf never");
            assert_eq!(result["ok"], false, "{result}");
            assert!(result["error"].is_string(), "{result}");
            assert!(result.get("pipeline_failures").is_none(), "{result}");
        }

        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let result = run_bash(&tools, "zzz | yyy & wait");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["exit_code"], 0, "{result}");
        let output = result["output"].as_str().unwrap();
        assert_eq!(output.matches("zzz").count(), 1, "{result}");
        assert_eq!(output.matches("yyy").count(), 1, "{result}");
        assert_eq!(output.lines().count(), 2, "{result}");
        assert_eq!(
            failed_stages(&result),
            [(1, 1, 127), (1, 2, 127)],
            "{result}"
        );
    }

    #[test]
    fn pipeline_live_three_stage_compound_and_function_streaming() {
        for producer in [
            r#"{ printf 'one\n'; until [ -f received ]; do sleep 0.02; done; printf 'two\n'; }"#,
            r#"live"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
            tools.limits.bash_timeout_seconds = 5;
            let body =
                r#"printf 'one\n'; until [ -f received ]; do sleep 0.02; done; printf 'two\n';"#;
            let command = format!(
                "live() {{ {body} }}; {producer} | cat | sh -c 'IFS= read -r first; printf \"%s\" \"$first\" > received; tail -n 1'"
            );
            let result = run_bash(&tools, &command);
            assert_eq!(result["ok"], true, "{result}");
            assert_eq!(result["output"], "two\n", "{result}");
            assert_eq!(
                fs::read_to_string(dir.path().join("received")).unwrap(),
                "one"
            );
        }
    }

    #[test]
    fn background_pipeline_preserves_only_final_output_and_failure_status() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let start = execute(
            &tools,
            "bash",
            json!({
                "mode":"background", "command":"{ printf 'hidden\nvisible\n'; exit 7; } | tail -n 1"
            }),
        );
        let id = start["task_id"].as_str().unwrap();
        let result = background_wait(&tools, id, |snapshot| snapshot["status"] != "running");
        assert_eq!(result["status"], "exited", "{result}");
        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(result["output"], "visible\n", "{result}");
        assert_eq!(failed_stages(&result), [(1, 1, 7)], "{result}");
        assert!(result.get("pipes").is_none(), "{result}");
    }
}
