use super::*;

#[test]
fn streaming_utf8_fragments_match_whole_lossy_decoding() {
    let cases: &[&[u8]] = &[
        "中😀".as_bytes(),
        b"a\xe4\xb8x\xf0\x9f\xff\x80z",
        b"\xf0\x9f\x98",
        b"\xe4\xf0\x9f\x98\x80",
        b"\xed\xa0\x80\xc0\xaf\xf4\x90\x80\x80",
        b"",
    ];
    for bytes in cases {
        for chunk_size in 1..=9 {
            let (text, live, truncated, received) = stream_capture(bytes, chunk_size);
            let expected = String::from_utf8_lossy(bytes);
            assert_eq!(text, expected, "chunk size {chunk_size}");
            assert_eq!(live, expected, "chunk size {chunk_size}");
            assert!(!truncated);
            assert_eq!(received, bytes.len());
        }
    }
}

#[test]
fn streaming_exact_cap_and_truncated_utf8_keep_live_and_result_equal() {
    for count in [OUTPUT_LIMIT - 1, OUTPUT_LIMIT, OUTPUT_LIMIT + 1] {
        let bytes = vec![b'x'; count];
        let (text, live, truncated, received) = stream_capture(&bytes, PIPE_CHUNK_SIZE);
        assert_eq!(received, count.min(OUTPUT_LIMIT));
        assert_eq!(text, "x".repeat(count.min(OUTPUT_LIMIT)));
        assert_eq!(live, text);
        assert_eq!(truncated, count > OUTPUT_LIMIT);
    }
    // The retained boundary can split a character. EOF of retained bytes
    // must produce the same single replacement as one-shot lossy decoding.
    for prefix in [OUTPUT_LIMIT - 1, OUTPUT_LIMIT - 2, OUTPUT_LIMIT - 3] {
        let mut bytes = vec![b'a'; prefix];
        bytes.extend_from_slice("中😀".as_bytes());
        bytes.resize(bytes.len() + OUTPUT_LIMIT * 2, b'z');
        for chunk_size in [1, 7, PIPE_CHUNK_SIZE] {
            let (text, live, truncated, received) = stream_capture(&bytes, chunk_size);
            assert_eq!(text, String::from_utf8_lossy(&bytes[..OUTPUT_LIMIT]));
            assert_eq!(live, text);
            assert!(truncated);
            assert_eq!(received, OUTPUT_LIMIT);
        }
    }
}

#[test]
fn streaming_bash_reports_both_pipes_before_exit_without_newlines() {
    let workflow = WorkflowState::default();
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let args = json!({
            "command":"printf 'out-first'; printf 'err-first' >&2; for ((i=0;i<200;i++)); do [[ -e release ]] && break; sleep 0.01; done; printf 'out-last'"
        })
        .to_string();
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut released = false;
    let mut started = false;
    let mut finished = false;
    let token = CancellationToken::new();
    let execution = tools.execute(
        "bash",
        &args,
        "live-call",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |event| match event {
            AgentEvent::ToolStarted {
                id,
                name,
                arguments,
            } => {
                assert!(!started && !finished);
                assert_eq!(id, "live-call");
                assert_eq!(name, "bash");
                assert_eq!(arguments, args);
                started = true;
            }
            AgentEvent::ToolOutput { id, stream, text } => {
                assert!(started && !finished);
                assert_eq!(id, "live-call");
                assert!(!text.is_empty());
                if !released {
                    assert!(!text.contains("out-last"), "output arrived only after exit");
                }
                match stream {
                    OutputStream::Stdout => stdout.push_str(&text),
                    OutputStream::Stderr => stderr.push_str(&text),
                }
                if !released && stdout == "out-first" && stderr == "err-first" {
                    fs::write(dir.path().join("release"), "").unwrap();
                    released = true;
                }
            }
            AgentEvent::ToolFinished { id, result } => {
                assert!(started && released && !finished);
                assert_eq!(id, "live-call");
                assert_eq!(result["stdout"], stdout);
                assert_eq!(result["stderr"], stderr);
                assert_eq!(result["exit_code"], 0);
                finished = true;
            }
            _ => panic!("unexpected assistant event"),
        },
    );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert!(released && finished);
    assert_eq!(stdout, "out-firstout-last");
    assert_eq!(stderr, "err-first");
    assert_eq!(result["ok"], true);
}
