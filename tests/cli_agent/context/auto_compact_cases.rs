use super::*;

#[test]
fn context_auto_compact_summarizes_old_prefix_inside_task_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        super::super::system_prompt::write(
            dir.path(),
            ".pp/SYSTEM_TEMPLATE.md",
            super::super::system_prompt::LIVE_TEMPLATE,
        );
        context_auto_write_growth_files(dir.path());
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(32768))],
            r#"{"default_model":"fixture/primary"}"#,
        );
        context_auto_grow_two_batches(protocol, &server, &mut cli, true);
        let (request, body) = protocol.receive(&server);
        context_assert_auto_summary_request(
            protocol,
            &body,
            &[AUTO_OLD_MARKER],
            &[AUTO_RECENT_MARKER, AUTO_REASONING, AUTO_ENCRYPTED],
        );
        super::super::system_prompt::assert_live(&body, protocol.name(), "execute");
        cli.wait_for(Pipe::Stderr, "Compacting context...");
        protocol.final_reply(request, AUTO_SUMMARY);
        cli.wait_for(Pipe::Stderr, "Context compacted: ~");

        let (request, body) = protocol.receive(&server);
        context_assert_auto_compacted_batch(
            protocol,
            &body,
            Some((AUTO_REASONING, AUTO_ENCRYPTED)),
            None,
        );
        super::super::system_prompt::assert_live(&body, protocol.name(), "execute");
        protocol.final_reply(request, "growth done");
        no_more(&server);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"growth done\n", "{protocol:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Compacting context..."), "{stderr}");
        assert!(stderr.contains("Context compacted: ~"), "{stderr}");
        // The internal summary never reaches stdout or the visible transcript.
        assert!(!stderr.contains(AUTO_SUMMARY), "{stderr}");
        // Each read ran exactly once; compaction never replayed the old batch.
        assert_eq!(stderr.matches("tool read: running").count(), 2, "{stderr}");
        let statuses = context_statuses(&stderr);
        assert_eq!(statuses.len(), 1, "{stderr}");
        assert!(statuses[0].contains(" / 32768 tokens ("), "{}", statuses[0]);
        assert!(
            statuses[0].contains("Auto compact: on at 80%"),
            "{}",
            statuses[0]
        );
        let statistics = super::super::stats::database(dir.path());
        let summaries: i64 = statistics
            .query_row(
                "SELECT COUNT(*) FROM requests WHERE purpose='auto_compact'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(summaries, 1);
        let entries: i64 = statistics.query_row("SELECT COUNT(*) FROM requests r JOIN request_payloads p ON r.id=p.request_id WHERE r.purpose='auto_compact' AND p.session_entry_json IS NOT NULL", [], |r|r.get(0)).unwrap();
        assert_eq!(entries, 0);
    }
}

#[test]
fn context_auto_compact_disabled_never_summarizes_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        context_auto_write_growth_files(dir.path());
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(32768))],
            r#"{"default_model":"fixture/primary","auto_compact":false}"#,
        );
        context_auto_grow_two_batches(protocol, &server, &mut cli, false);
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        assert!(body.to_string().contains(AUTO_RECENT_MARKER), "{body}");
        protocol.final_reply(request, "off grown");
        cli.wait_for(Pipe::Stdout, "off grown");

        cli.input("off still\n");
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        let wire = body.to_string();
        assert!(!wire.contains("[Context summary]"), "{body}");
        assert!(wire.contains(AUTO_OLD_MARKER), "{body}");
        assert!(wire.contains(AUTO_RECENT_MARKER), "{body}");
        protocol.final_reply(request, "off done");
        no_more(&server);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"off grown\noff done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("Compacting context..."), "{stderr}");
        assert!(!stderr.contains("[Context summary]"), "{stderr}");
    }
}

#[test]
fn context_auto_compact_threshold_tracks_the_actual_estimate_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        context_auto_write_growth_files(dir.path());
        // Calibration: the open model has no context window, so the identical
        // history and prompt produce the trigger request without compacting.
        let mut calib = protocol.start(
            &server,
            dir.path(),
            &[("open", None)],
            r#"{"default_model":"fixture/open","auto_compact":false}"#,
        );
        context_auto_grow_two_batches(protocol, &server, &mut calib, false);
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        protocol.final_reply(request, "boundary grown");
        calib.wait_for(Pipe::Stdout, "boundary grown");
        calib.input("boundary task\n");
        let (request, _body, bytes) = context_receive_raw(protocol, &server);
        protocol.final_reply(request, "boundary measured");
        calib.wait_for(Pipe::Stdout, "boundary measured");
        calib.input("/quit\n");
        let output = calib.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let estimate = bytes.div_ceil(3);
        let window = 65536u64;
        let quotient = estimate * 100 / window;
        assert!(
            (2..=97).contains(&quotient),
            "{protocol:?}: estimate {estimate} is not usable"
        );
        let trigger_threshold = quotient.saturating_sub(1).max(1) as u8;
        // Bracket the measured boundary from both sides even if the variant's
        // model field shifts the estimate by a token.
        let quiet_threshold = (quotient + 2) as u8;
        assert!(quiet_threshold <= 99, "{quiet_threshold}");

        for (threshold, summary_expected) in [(trigger_threshold, true), (quiet_threshold, false)] {
            let run_dir = tempfile::tempdir().unwrap();
            context_auto_write_growth_files(run_dir.path());
            let settings = format!(
                r#"{{"default_model":"fixture/open","auto_compact_threshold_percent":{threshold}}}"#
            );
            let mut cli = protocol.start(
                &server,
                run_dir.path(),
                &[("open", None), ("tight", Some(window))],
                &settings,
            );
            context_auto_grow_two_batches(protocol, &server, &mut cli, false);
            let (request, body) = protocol.receive(&server);
            // The open model keeps automatic compaction inactive even though
            // the history already exceeds 80% of the other model's window.
            assert!(context_has_tools(&body), "{body}");
            protocol.final_reply(request, "boundary grown");
            cli.wait_for(Pipe::Stdout, "boundary grown");

            cli.input("/model fixture/tight\n");
            cli.input("boundary task\n");
            let (request, body) = protocol.receive(&server);
            if summary_expected {
                context_assert_auto_summary_request(
                    protocol,
                    &body,
                    &[AUTO_OLD_MARKER],
                    &["boundary task"],
                );
                protocol.final_reply(request, AUTO_SUMMARY);
                let (request, body) = protocol.receive(&server);
                context_assert_auto_compacted_tail(
                    protocol,
                    &body,
                    "boundary grown",
                    "boundary task",
                );
                protocol.final_reply(request, "boundary done");
            } else {
                assert!(
                    context_has_tools(&body),
                    "threshold {threshold} unexpectedly compacted: {body}"
                );
                let wire = body.to_string();
                assert!(!wire.contains("[Context summary]"), "{body}");
                assert!(wire.contains(AUTO_OLD_MARKER), "{body}");
                assert!(wire.contains(AUTO_RECENT_MARKER), "{body}");
                protocol.final_reply(request, "boundary done");
            }
            no_more(&server);
            cli.input("/quit\n");
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{protocol:?} threshold {threshold}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                output.stdout, b"boundary grown\nboundary done\n",
                "{protocol:?} threshold {threshold}"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                stderr.contains("Compacting context..."),
                summary_expected,
                "{protocol:?} threshold {threshold}: {stderr}"
            );
        }
    }
}

#[test]
fn context_auto_compact_capacity_gate_follows_the_current_model_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        context_auto_write_growth_files(dir.path());
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("open", None), ("tight", Some(80_000))],
            r#"{"default_model":"fixture/open","rate_limit_retry_budget_seconds":0}"#,
        );

        // Five large batches put the history over 80% of the configured
        // model's window while the current model has no window.
        context_auto_grow_batches(protocol, &server, &mut cli, 5);
        cli.input("open normal\n");
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        let wire = body.to_string();
        assert!(!wire.contains("[Context summary]"), "{body}");
        assert!(wire.contains(AUTO_OLD_MARKER), "{body}");
        protocol.final_reply(request, "open normal done");
        cli.wait_for(Pipe::Stdout, "open normal done");

        // Selecting the configured model immediately enables the 80% trigger.
        // The summary is failed deliberately so the over-threshold history
        // stays native for the next model switch.
        cli.input("/model fixture/tight\n");
        cli.input("tight auto\n");
        let (request, body) = protocol.receive(&server);
        context_assert_auto_summary_request(protocol, &body, &[AUTO_OLD_MARKER], &["tight auto"]);
        ManualFailure::Blank.reply(protocol, request);
        cli.wait_for(Pipe::Stderr, AutoSummaryFailure::Blank.expected());
        no_more(&server);

        // Back on the unconfigured model the same history is never summarized.
        cli.input("/model fixture/open\n");
        cli.input("open again\n");
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        let wire = body.to_string();
        assert!(!wire.contains("[Context summary]"), "{body}");
        assert!(wire.contains(AUTO_OLD_MARKER), "{body}");
        protocol.final_reply(request, "open again done");
        cli.wait_for(Pipe::Stdout, "open again done");

        // Selecting the configured model again re-enables the same trigger.
        cli.input("/model fixture/tight\n");
        cli.input("tight again\n");
        let (request, body) = protocol.receive(&server);
        context_assert_auto_summary_request(protocol, &body, &[AUTO_OLD_MARKER], &["tight again"]);
        ManualFailure::Blank.reply(protocol, request);
        cli.wait_for(Pipe::Stderr, AutoSummaryFailure::Blank.expected());
        no_more(&server);

        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout,
            b"open grown\nopen normal done\nopen again done\n"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            stderr.matches("Compacting context...").count(),
            2,
            "{stderr}"
        );
        assert!(!stderr.contains("Context compacted:"), "{stderr}");
    }
}

#[test]
fn context_manual_compact_still_works_without_configured_window_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        context_write_large_file(dir.path());
        // Automatic compaction is enabled but cannot know the capacity; the
        // explicit command has no such gate.
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", None)],
            r#"{"default_model":"fixture/primary"}"#,
        );
        context_seed_history(protocol, &server, &mut cli);
        cli.input("/compact\n");
        let (request, body) = protocol.receive(&server);
        context_assert_auto_summary_request(
            protocol,
            &body,
            &[OLD_USER],
            &["recent task", "recent done"],
        );
        protocol.final_reply(request, SUMMARY_TEXT);
        cli.wait_for(Pipe::Stderr, "Context compacted: ~");
        no_more(&server);
        cli.input("next task\n");
        let (request, body) = protocol.receive(&server);
        context_assert_compacted_history(protocol, &body, "next task");
        protocol.final_reply(request, "next done");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"alpha noted\nrecent done\nnext done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(" / ? tokens (estimated)"), "{stderr}");
        assert!(
            stderr.contains("inactive (context_window not configured)"),
            "{stderr}"
        );
        assert!(!stderr.contains(SUMMARY_TEXT), "{stderr}");
    }
}

#[test]
fn context_resume_capacity_comes_from_models_not_the_saved_log_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        // Each batch stays below the retained-suffix budget, so the last
        // batch remains native and the summary request fits the window.
        context_auto_write_growth_files_sized(dir.path(), 38_000);
        // Save an over-threshold session with capacity configured but the
        // automatic trigger disabled.
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(32768))],
            r#"{"default_model":"fixture/primary","auto_compact":false}"#,
        );
        context_auto_grow_two_batches(protocol, &server, &mut cli, false);
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        protocol.final_reply(request, "saved grown");
        cli.wait_for(Pipe::Stdout, "saved grown");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let id = context_session_id(&String::from_utf8_lossy(&output.stderr));

        // Removing the capacity from the current model config, then resuming,
        // must not reuse a window inferred from the saved log.
        std::fs::write(
            config_path(dir.path(), "models.yml"),
            protocol.models(&server, &[("primary", None)]),
        )
        .unwrap();
        // 偏好已由 agent.db 权威；这里只把自动压缩恢复为默认开启。
        context_auto_rewrite_settings(dir.path(), r#"{"auto_compact":true}"#);
        let resumed = context_auto_resume(dir.path(), &id, "no stale capacity");
        let (request, body) = protocol.receive(&server);
        context_assert_no_summary_with(
            protocol,
            &body,
            "no stale capacity",
            &[AUTO_OLD_MARKER, AUTO_RECENT_MARKER],
        );
        protocol.final_reply(request, "no stale done");
        let output = resumed.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"no stale done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(" / ? tokens (estimated)"), "{stderr}");
        assert!(
            stderr.contains("inactive (context_window not configured)"),
            "{stderr}"
        );
        assert!(!stderr.contains("Compacting context..."), "{stderr}");
        no_more(&server);

        // Restoring the capacity in the current catalog enables the trigger
        // immediately for the same saved history.
        std::fs::write(
            config_path(dir.path(), "models.yml"),
            protocol.models(&server, &[("primary", Some(32768))]),
        )
        .unwrap();
        let restored = context_auto_resume(dir.path(), &id, "capacity restored");
        let (request, body) = protocol.receive(&server);
        context_assert_auto_summary_request(
            protocol,
            &body,
            &[AUTO_OLD_MARKER, AUTO_RECENT_MARKER],
            &["capacity restored"],
        );
        protocol.final_reply(request, AUTO_SUMMARY);
        let (request, body) = protocol.receive(&server);
        // The batches were summarized; only the trailing exchanges remain.
        assert!(context_has_tools(&body), "{body}");
        let wire = body.to_string();
        assert!(!wire.contains(AUTO_OLD_MARKER), "{body}");
        assert!(!wire.contains(AUTO_RECENT_MARKER), "{body}");
        assert_eq!(wire.matches("[Context summary]").count(), 1, "{body}");
        match protocol {
            Protocol::Chat => {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 6, "{body}");
                assert_eq!(messages[0]["role"], "system");
                slash_assert_message(&messages[1], "user", &context_auto_summary_wrap());
                slash_assert_message(&messages[2], "assistant", "saved grown");
                slash_assert_message(&messages[3], "user", "no stale capacity");
                slash_assert_message(&messages[4], "assistant", "no stale done");
                slash_assert_message(&messages[5], "user", "capacity restored");
            }
            Protocol::Responses => {
                let input = body["input"].as_array().unwrap();
                assert_eq!(input.len(), 5, "{body}");
                response_user(&input[0], &context_auto_summary_wrap());
                slash_assert_message(&input[1], "assistant", "saved grown");
                response_user(&input[2], "no stale capacity");
                slash_assert_message(&input[3], "assistant", "no stale done");
                response_user(&input[4], "capacity restored");
            }
        }
        protocol.final_reply(request, "restored done");
        let output = restored.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"restored done\n");
        no_more(&server);
    }
}

#[test]
fn context_auto_compact_without_old_prefix_fails_without_network_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(32768))],
            r#"{"default_model":"fixture/primary"}"#,
        );
        let prompt = format!("NO-PREFIX-MARKER {}", "p".repeat(90_000));
        cli.input(&format!("{prompt}\n"));
        cli.wait_for(
            Pipe::Stderr,
            "cannot compact: retained context exceeds automatic threshold",
        );
        no_more(&server);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{protocol:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("Compacting context..."), "{stderr}");
        let id = context_session_id(&stderr);
        // The accepted prompt was journaled before the local failure.
        let log = std::fs::read_to_string(context_session_path(dir.path(), &id)).unwrap();
        assert!(log.contains("NO-PREFIX-MARKER"), "{log}");

        context_auto_rewrite_settings(dir.path(), r#"{"auto_compact":false}"#);
        let recovered = context_auto_resume(dir.path(), &id, "after no prefix");
        let (request, body) = protocol.receive(&server);
        context_assert_no_summary_with(protocol, &body, "after no prefix", &["NO-PREFIX-MARKER"]);
        protocol.final_reply(request, "no prefix recovered");
        let output = recovered.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"no prefix recovered\n");
        no_more(&server);
    }
}

#[test]
fn context_auto_compact_single_oversized_exchange_fails_without_network_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(32768))],
            r#"{"default_model":"fixture/primary"}"#,
        );
        cli.input("seed exchange\n");
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        let answer = format!("OVERSIZED-EXCHANGE-MARKER {}", "e".repeat(90_000));
        protocol.final_reply(request, &answer);
        cli.wait_for(Pipe::Stdout, "OVERSIZED-EXCHANGE-MARKER");
        cli.input("trigger oversized\n");
        cli.wait_for(
            Pipe::Stderr,
            "cannot compact: retained context exceeds automatic threshold",
        );
        no_more(&server);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("Compacting context..."), "{stderr}");
        let id = context_session_id(&stderr);
        let log = std::fs::read_to_string(context_session_path(dir.path(), &id)).unwrap();
        assert!(log.contains("OVERSIZED-EXCHANGE-MARKER"), "{log}");
        assert!(log.contains("trigger oversized"), "{log}");

        context_auto_rewrite_settings(dir.path(), r#"{"auto_compact":false}"#);
        let recovered = context_auto_resume(dir.path(), &id, "after oversized");
        let (request, body) = protocol.receive(&server);
        context_assert_no_summary_with(
            protocol,
            &body,
            "after oversized",
            &["OVERSIZED-EXCHANGE-MARKER", "trigger oversized"],
        );
        protocol.final_reply(request, "oversized recovered");
        let output = recovered.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"oversized recovered\n");
        no_more(&server);
    }
}

#[test]
fn context_auto_compact_rejects_retained_context_over_threshold_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        context_auto_write_file(dir.path(), "retained-small.txt", RETAINED_SMALL_MARKER, 400);
        context_auto_write_file(
            dir.path(),
            "retained-huge.txt",
            RETAINED_HUGE_MARKER,
            65_000,
        );
        // 80% of 24000 is 19200 estimated tokens: the huge last batch alone
        // exceeds it, so summarizing the small old exchange cannot succeed.
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(24000))],
            r#"{"default_model":"fixture/primary"}"#,
        );
        cli.input("grow retained\n");
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        context_auto_calls(
            protocol,
            request,
            "retained-old",
            "retained-small.txt",
            None,
        );
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        assert!(body.to_string().contains(RETAINED_SMALL_MARKER), "{body}");
        context_auto_calls(
            protocol,
            request,
            "retained-recent",
            "retained-huge.txt",
            None,
        );
        let (request, body) = protocol.receive(&server);
        context_assert_auto_summary_request(
            protocol,
            &body,
            &[RETAINED_SMALL_MARKER],
            &[RETAINED_HUGE_MARKER],
        );
        protocol.final_reply(request, "SUMMARY: retained");
        cli.wait_for(
            Pipe::Stderr,
            "cannot compact: retained context exceeds automatic threshold",
        );
        no_more(&server);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{protocol:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("SUMMARY: retained"), "{stderr}");
        assert_eq!(stderr.matches("tool read: running").count(), 2, "{stderr}");
        let id = context_session_id(&stderr);

        context_auto_rewrite_settings(dir.path(), r#"{"auto_compact":false}"#);
        let recovered = context_auto_resume(dir.path(), &id, "after retained");
        let (request, body) = protocol.receive(&server);
        context_assert_no_summary_with(
            protocol,
            &body,
            "after retained",
            &[RETAINED_SMALL_MARKER, RETAINED_HUGE_MARKER],
        );
        protocol.final_reply(request, "retained recovered");
        let output = recovered.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"retained recovered\n");
        no_more(&server);
    }
}

#[test]
fn context_auto_compact_rejects_system_over_threshold_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        // A huge enabled-skill description inflates the system instructions;
        // tools alone would not, so this scenarios the fixed request overhead.
        let skill = config_path(home.path(), "skills").join("bulk");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!(
                "---\nname: bulk\ndescription: {}\n---\nBulk skill body.\n",
                "z".repeat(100_000)
            ),
        )
        .unwrap();

        // Measure the exact fixed overhead once with automatic compaction off.
        let mut measure = protocol.start(
            &server,
            home.path(),
            &[("open", None)],
            r#"{"default_model":"fixture/open","auto_compact":false}"#,
        );
        measure.input("seed one\n");
        let (request, body, bytes) = context_receive_raw(protocol, &server);
        assert!(context_has_tools(&body), "{body}");
        assert!(body.to_string().contains("zzzz"), "{body}");
        protocol.final_reply(request, "one");
        measure.wait_for(Pipe::Stdout, "one");
        measure.input("/quit\n");
        let output = measure.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let window = bytes.div_ceil(3) + 6000;
        std::fs::write(
            config_path(home.path(), "models.yml"),
            protocol.models(&server, &[("open", None), ("tight", Some(window))]),
        )
        .unwrap();
        // 偏好已由 agent.db 权威；这里只把自动压缩恢复为默认开启。
        context_auto_rewrite_settings(home.path(), r#"{"auto_compact":true}"#);

        let mut cli = LiveCli::new(Cli::start_with_existing_files(home.path(), None));
        cli.input("seed one\n");
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        protocol.final_reply(request, "one");
        cli.wait_for(Pipe::Stdout, "one");
        cli.input("seed two\n");
        let (request, body) = protocol.receive(&server);
        assert!(context_has_tools(&body), "{body}");
        protocol.final_reply(request, "two");
        cli.wait_for(Pipe::Stdout, "two");

        cli.input("/model fixture/tight\n");
        cli.input("trigger system\n");
        let (request, body) = protocol.receive(&server);
        context_assert_auto_summary_request(
            protocol,
            &body,
            &["seed one"],
            &["seed two", "trigger system"],
        );
        protocol.final_reply(request, "SUMMARY: system");
        cli.wait_for(
            Pipe::Stderr,
            "cannot compact: retained context exceeds automatic threshold",
        );
        no_more(&server);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"one\ntwo\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("SUMMARY: system"), "{stderr}");
        let id = context_session_id(&stderr);

        context_auto_rewrite_settings(home.path(), r#"{"auto_compact":false}"#);
        let recovered = context_auto_resume(home.path(), &id, "after system");
        let (request, body) = protocol.receive(&server);
        context_assert_no_summary_with(protocol, &body, "after system", &["trigger system"]);
        protocol.final_reply(request, "system recovered");
        let output = recovered.finish();
        assert!(
            output.status.success(),
            "{protocol:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"system recovered\n");
        no_more(&server);
    }
}

#[test]
fn context_auto_compact_failures_never_retry_or_send_the_normal_request() {
    for protocol in PROTOCOLS {
        for failure in AUTO_SUMMARY_FAILURES {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            context_auto_write_growth_files(dir.path());
            let mut cli = protocol.start(
                &server,
                dir.path(),
                &[("primary", Some(32768))],
                r#"{"default_model":"fixture/primary","rate_limit_retry_budget_seconds":0}"#,
            );
            context_auto_grow_two_batches(protocol, &server, &mut cli, true);
            let (request, body) = protocol.receive(&server);
            context_assert_auto_summary_request(
                protocol,
                &body,
                &[AUTO_OLD_MARKER],
                &[AUTO_RECENT_MARKER, AUTO_REASONING, AUTO_ENCRYPTED],
            );
            failure.reply(protocol, request);
            cli.wait_for(Pipe::Stderr, failure.expected());
            // No retried summary and no ordinary request after the failure.
            no_more(&server);
            cli.input("/quit\n");
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{failure:?} {protocol:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stdout.is_empty(),
                "{failure:?} {protocol:?}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                stderr.matches("tool read: running").count(),
                2,
                "{failure:?} {protocol:?}: {stderr}"
            );
            assert!(
                !stderr.contains(AUTO_SUMMARY),
                "{failure:?} {protocol:?}: {stderr}"
            );
            let id = context_session_id(&stderr);

            // The accepted prompt and both tool batches are durable enough to
            // recover once the automatic trigger is disabled.
            context_auto_rewrite_settings(dir.path(), r#"{"auto_compact":false}"#);
            let recovered = context_auto_resume(dir.path(), &id, "after auto failure");
            let (request, body) = protocol.receive(&server);
            context_assert_auto_uncompacted(protocol, &body, "after auto failure");
            protocol.final_reply(request, "recovered");
            let output = recovered.finish();
            assert!(
                output.status.success(),
                "{failure:?} {protocol:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"recovered\n");
            no_more(&server);
        }
    }
}
