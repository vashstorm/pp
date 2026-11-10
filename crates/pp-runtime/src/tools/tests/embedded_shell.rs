use super::*;
use std::os::unix::fs::PermissionsExt;

fn run_bash(tools: &Tools, command: &str) -> Value {
    execute(tools, "bash", json!({"command":command}))
}

fn file_mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn embedded_shell_foreground_streams_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let success = run_bash(&tools, "printf ok");
    assert_eq!(success["ok"], true, "{success}");
    assert_eq!(success["exit_code"], 0, "{success}");
    assert_eq!(success["output"], "ok", "{success}");

    let failure = run_bash(&tools, "printf 'out'; printf 'err' >&2; exit 3");
    assert_eq!(failure["ok"], false, "{failure}");
    assert_eq!(failure["exit_code"], 3, "{failure}");
    assert_eq!(failure["timed_out"], false, "{failure}");
    assert_eq!(failure["cancelled"], false, "{failure}");
    let output = failure["output"].as_str().unwrap();
    assert!(output.contains("out"), "{failure}");
    assert!(output.contains("err"), "{failure}");

    // The host survives a failed run and the next call gets a fresh logical shell.
    let next = run_bash(&tools, "printf next");
    assert_eq!(next["ok"], true, "{next}");
    assert_eq!(next["output"], "next", "{next}");
}

#[test]
fn embedded_shell_named_pipe_redirects_fail_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(dir.path().join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    std::os::unix::fs::symlink("fifo", dir.path().join("link")).unwrap();
    let root = dir.path().to_owned();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let tools = Tools::new(root, EditMode::Replace).unwrap();
        for command in ["cat < fifo", "printf x > link", "printf x &>> fifo"] {
            send.send(run_bash(&tools, command)).unwrap();
        }
        send.send(run_bash(
            &tools,
            "bash -c 'printf fifo-ok > fifo & cat < fifo; wait'",
        ))
        .unwrap();
    });

    for _ in 0..3 {
        let result = receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(result["ok"], false, "{result}");
        assert!(
            result
                .to_string()
                .contains("named pipe redirection requires an external shell"),
            "{result}"
        );
    }
    let external = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(external["ok"], true, "{external}");
    assert_eq!(external["output"], "fifo-ok", "{external}");
    worker.join().unwrap();
}

#[test]
fn embedded_shell_large_printf_width_pads_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    tools.artifacts.bind(dir.path(), "embedded").unwrap();

    let result = run_bash(&tools, "printf '%70000s' ''; printf marker");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["timed_out"], false, "{result}");
    assert_eq!(result["truncated"], true, "{result}");
    assert!(
        result["output"].as_str().unwrap().len() <= 50 * 1024 + 128,
        "{result}"
    );
    assert_eq!(result["artifact"]["complete"], true, "{result}");
    let log = fs::read(dir.path().join("embedded/0.bash.log")).unwrap();
    assert_eq!(
        log.len(),
        70_000 + "marker".len(),
        "wide printf output was not captured in full"
    );
    assert!(log[..70_000].iter().all(|byte| *byte == b' '));
    assert_eq!(&log[70_000..], b"marker");

    // Alignment, zero padding and `*` widths keep their printf semantics.
    let written = run_bash(
        &tools,
        "printf '%-70000s' 'x' > left.txt; \
         printf '%070000d' 7 > zero.txt; \
         printf '%+070000d' 7 > sign.txt; \
         printf '%*s' 70000 '' > star.txt; \
         printf '%70000.3f' 1.5 > float.txt; \
         printf '%70000d' 7 > decimal.txt; \
         printf '%70000.*f' 2 3.14159 > dynamic-precision.txt; \
         printf '%2$70000s' ignored value > positional.txt; \
         printf '%70000s%70000s' a b > multi.txt; \
         printf '%70000s|' a b > batches.txt; \
         printf '%070000f' inf > nonfinite.txt",
    );
    assert_eq!(written["ok"], true, "{written}");
    let left = fs::read_to_string(dir.path().join("left.txt")).unwrap();
    assert_eq!(left.len(), 70_000);
    assert!(left.starts_with('x'));
    assert!(left[1..].bytes().all(|byte| byte == b' '));
    let zero = fs::read_to_string(dir.path().join("zero.txt")).unwrap();
    assert_eq!(zero.len(), 70_000);
    assert!(zero.ends_with('7'));
    assert!(zero[..69_999].bytes().all(|byte| byte == b'0'));
    let sign = fs::read_to_string(dir.path().join("sign.txt")).unwrap();
    assert_eq!(sign.len(), 70_000);
    assert!(sign.starts_with("+0"));
    assert!(sign.ends_with('7'));
    let star = fs::read_to_string(dir.path().join("star.txt")).unwrap();
    assert_eq!(star.len(), 70_000);
    assert!(star.bytes().all(|byte| byte == b' '));
    let float = fs::read_to_string(dir.path().join("float.txt")).unwrap();
    assert_eq!(float.len(), 70_000);
    assert!(float.ends_with("1.500"));
    let decimal = fs::read_to_string(dir.path().join("decimal.txt")).unwrap();
    assert_eq!(decimal.len(), 70_000);
    assert_eq!(decimal, format!("{}7", " ".repeat(69_999)));
    let dynamic_precision = fs::read_to_string(dir.path().join("dynamic-precision.txt")).unwrap();
    assert_eq!(dynamic_precision.len(), 70_000);
    assert!(dynamic_precision.ends_with("3.14"));
    let positional = fs::read_to_string(dir.path().join("positional.txt")).unwrap();
    assert_eq!(positional.len(), 70_000);
    assert!(positional.ends_with("value"));
    let multi = fs::read_to_string(dir.path().join("multi.txt")).unwrap();
    assert_eq!(multi.len(), 140_000);
    assert_eq!(multi.find('a'), Some(69_999));
    assert_eq!(multi.find('b'), Some(139_999));
    let batches = fs::read_to_string(dir.path().join("batches.txt")).unwrap();
    assert_eq!(batches.len(), 140_002);
    assert_eq!(batches.as_bytes()[69_999], b'a');
    assert_eq!(batches.as_bytes()[70_000], b'|');
    assert_eq!(batches.as_bytes()[140_001], b'|');
    let nonfinite = fs::read_to_string(dir.path().join("nonfinite.txt")).unwrap();
    assert_eq!(nonfinite.len(), 70_000);
    assert!(nonfinite.ends_with("inf"));
    assert!(nonfinite[..69_997].bytes().all(|byte| byte == b' '));

    // The host formatter handles widths up to 65535; wider fields take the
    // manual padding path without changing the byte count.
    let boundary = run_bash(
        &tools,
        "printf '%65535s' '' > boundary-65535.txt; \
         printf '%65536s' '' > boundary-65536.txt; \
         printf '%*s' 65536 '' > boundary-asterisk.txt",
    );
    assert_eq!(boundary["ok"], true, "{boundary}");
    for name in [
        "boundary-65535.txt",
        "boundary-65536.txt",
        "boundary-asterisk.txt",
    ] {
        let bytes = fs::metadata(dir.path().join(name)).unwrap().len();
        assert_eq!(bytes, 65_536 - u64::from(name.contains("65535")), "{name}");
    }
    // uucore still refuses widths whose padding exceeds its own limit.
    let oversized = run_bash(&tools, "printf '%2000000s' ''");
    assert_eq!(oversized["ok"], false, "{oversized}");
    assert_eq!(oversized["timed_out"], false, "{oversized}");

    let next = run_bash(&tools, "printf next");
    assert_eq!(next["output"], "next", "{next}");
}

#[test]
fn embedded_shell_printf_large_precision_pads_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(
        &tools,
        "printf '%.65535d' 7 > int-65535.txt; \
         printf '%.65536d' 7 > int-65536.txt; \
         printf '%.70000d' 7 > int-70000.txt; \
         printf '%.70000d' -7 > int-negative.txt; \
         printf '%#.70000x' 255 > hex.txt; \
         printf '%#.70000o' 8 > octal.txt; \
         printf '%.65535s' abc > string-65535.txt; \
         printf '%.65536s' abc > string-65536.txt; \
         printf '%.70000s' abc > string-70000.txt; \
         printf '%.65535f' 1.5 > float-65535.txt; \
         printf '%.65536f' 1.5 > float-65536.txt; \
         printf '%.70000f' 1.5 > float-70000.txt; \
         printf '%.70000f' -1.5 > float-negative.txt; \
         printf '%.70000f' 0 > float-zero.txt; \
         printf '%.70000f' 7 > float-integer.txt; \
         printf '%.65535f' 0 > float-zero-65535.txt; \
         printf '%.65536f' 0 > float-zero-65536.txt; \
         printf '%.70000e' 1.5 > exp.txt; \
         printf '%.70000e' 0 > exp-zero.txt; \
         printf '%#.70000g' 0 > shortest-zero.txt; \
         printf '%.70000g' 1.5 > shortest.txt; \
         printf '%.*d' 65535 7 > star-65535.txt; \
         printf '%.*d' 65536 7 > star-65536.txt; \
         printf '%.*d' 70000 7 > star-70000.txt; \
         printf '%.*f' 70000 1.5 > star-float.txt; \
         printf '%.*s' 70000 abc > star-string.txt; \
         printf '%70000.70000d' 7 > width-precision.txt; \
         printf '%080000.70000d' 7 > width-zero-precision.txt",
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");

    let read = |name: &str| fs::read(dir.path().join(name)).unwrap();
    let assert_zeros_then = |bytes: &[u8], suffix: &[u8]| {
        assert!(bytes.ends_with(suffix), "{suffix:?} missing");
        assert!(
            bytes[..bytes.len() - suffix.len()]
                .iter()
                .all(|byte| *byte == b'0'),
            "non-zero padding"
        );
    };

    let int_65535 = read("int-65535.txt");
    assert_eq!(int_65535.len(), 65_535);
    assert_zeros_then(&int_65535, b"7");
    let int_65536 = read("int-65536.txt");
    assert_eq!(int_65536.len(), 65_536);
    assert_zeros_then(&int_65536, b"7");
    let int_70000 = read("int-70000.txt");
    assert_eq!(int_70000.len(), 70_000);
    assert_zeros_then(&int_70000, b"7");
    let int_negative = read("int-negative.txt");
    assert_eq!(int_negative.len(), 70_001);
    assert_eq!(int_negative[0], b'-');
    assert_zeros_then(&int_negative[1..], b"7");

    let hex = read("hex.txt");
    assert_eq!(hex.len(), 70_002);
    assert!(hex.starts_with(b"0x"));
    assert!(hex.ends_with(b"ff"));
    assert!(hex[2..70_000].iter().all(|byte| *byte == b'0'));
    let octal = read("octal.txt");
    assert_eq!(octal.len(), 70_000);
    assert!(octal.ends_with(b"10"));
    assert!(octal[..69_998].iter().all(|byte| *byte == b'0'));

    for name in [
        "string-65535.txt",
        "string-65536.txt",
        "string-70000.txt",
        "star-string.txt",
    ] {
        assert_eq!(read(name), b"abc", "{name}");
    }

    let float_65535 = read("float-65535.txt");
    assert_eq!(float_65535.len(), 65_537);
    assert!(float_65535.starts_with(b"1.5"));
    assert!(float_65535[3..].iter().all(|byte| *byte == b'0'));
    let float_65536 = read("float-65536.txt");
    assert_eq!(float_65536.len(), 65_538);
    assert!(float_65536.starts_with(b"1.5"));
    assert!(float_65536[3..].iter().all(|byte| *byte == b'0'));
    let float_70000 = read("float-70000.txt");
    assert_eq!(float_70000.len(), 70_002);
    assert!(float_70000.starts_with(b"1.5"));
    assert!(float_70000[3..].iter().all(|byte| *byte == b'0'));
    let float_negative = read("float-negative.txt");
    assert_eq!(float_negative.len(), 70_003);
    assert!(float_negative.starts_with(b"-1.5"));
    assert!(float_negative[4..].iter().all(|byte| *byte == b'0'));
    // bigdecimal stops adding integer padding beyond its guard, so a zero or
    // integer-valued `%f` remains "0"/"7" at any large precision.
    assert_eq!(read("float-zero.txt"), b"0");
    assert_eq!(read("float-zero-65535.txt"), b"0");
    assert_eq!(read("float-zero-65536.txt"), b"0");
    assert_eq!(read("float-integer.txt"), b"7");

    let exp = read("exp.txt");
    assert_eq!(exp.len(), 70_006);
    assert!(exp.starts_with(b"1.5"));
    assert!(exp.ends_with(b"e+00"));
    assert!(exp[3..70_002].iter().all(|byte| *byte == b'0'));
    let exp_zero = read("exp-zero.txt");
    assert_eq!(exp_zero.len(), 70_006);
    assert!(exp_zero.starts_with(b"0."));
    assert!(exp_zero.ends_with(b"e+00"));
    assert!(exp_zero[2..70_002].iter().all(|byte| *byte == b'0'));

    let shortest_zero = read("shortest-zero.txt");
    assert_eq!(shortest_zero.len(), 70_001);
    assert!(shortest_zero.starts_with(b"0."));
    assert!(shortest_zero[2..].iter().all(|byte| *byte == b'0'));
    assert_eq!(read("shortest.txt"), b"1.5");

    let star_65535 = read("star-65535.txt");
    assert_eq!(star_65535.len(), 65_535);
    assert_zeros_then(&star_65535, b"7");
    let star_65536 = read("star-65536.txt");
    assert_eq!(star_65536.len(), 65_536);
    assert_zeros_then(&star_65536, b"7");
    let star_70000 = read("star-70000.txt");
    assert_eq!(star_70000.len(), 70_000);
    assert_zeros_then(&star_70000, b"7");
    let star_float = read("star-float.txt");
    assert_eq!(star_float.len(), 70_002);
    assert!(star_float.starts_with(b"1.5"));
    assert!(star_float[3..].iter().all(|byte| *byte == b'0'));

    let width_precision = read("width-precision.txt");
    assert_eq!(width_precision.len(), 70_000);
    assert_zeros_then(&width_precision, b"7");
    let width_zero_precision = read("width-zero-precision.txt");
    assert_eq!(width_zero_precision.len(), 80_000);
    assert!(
        width_zero_precision[..10_000]
            .iter()
            .all(|byte| *byte == b' ')
    );
    assert_zeros_then(&width_zero_precision[10_000..], b"7");

    // The safety bound still rejects precision that would allocate unbounded
    // padding, while string truncation precision stays unbounded (no padding).
    let oversized = run_bash(&tools, "printf '%.1000001d' 7");
    assert_eq!(oversized["ok"], false, "{oversized}");
    assert_eq!(oversized["timed_out"], false, "{oversized}");
    let long_string_precision = run_bash(&tools, "printf '%.1000001s' abc");
    assert_eq!(long_string_precision["ok"], true, "{long_string_precision}");
    assert_eq!(
        long_string_precision["output"], "abc",
        "{long_string_precision}"
    );

    let next = run_bash(&tools, "printf next");
    assert_eq!(next["ok"], true, "{next}");
    assert_eq!(next["output"], "next", "{next}");
}

#[test]
fn embedded_shell_printf_asterisk_widths_keep_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    for (command, expected) in [
        ("printf '%*s' 5 a", "    a"),
        ("printf '%-*s' 5 a", "a    "),
        ("printf '%*s' -5 a", "a    "),
        ("printf '%*s|' 3 a 4 b", "  a|   b|"),
        ("printf '%%%*s' 4 x", "%   x"),
        ("printf '%*d' 6 -42", "   -42"),
        ("printf '%0*d' 6 -42", "-00042"),
        ("printf '%+0*d' 6 42", "+00042"),
        ("printf '%#0*x' 6 255", "0x00ff"),
        ("printf '%#0*o' 6 8", "000010"),
        ("printf '%*.*f' 10 2 3.14159", "      3.14"),
        ("printf '%0*f' 10 -3.5", "-03.500000"),
        ("printf '%*.*s' 8 2 abcdef", "      ab"),
        ("printf '%.*d|' 3 1 4 2", "001|0002|"),
        ("printf '%.*f|' 2 1.5 3 2.5", "1.50|2.500|"),
        ("printf '%2$.*1$f' 2 1.5", "1.50"),
        ("printf '%2$*1$s' 5 a", "    a"),
        ("printf '%*2$s' x 5", "    x"),
        ("printf '%*c' 3 xyz", "  x"),
        ("printf '%0*f' 10 inf", "       inf"),
        ("printf '%+0*f' 10 inf", "      +inf"),
        ("printf '%0*F' 10 -inf", "      -INF"),
        ("printf '%0*f' 10 nan", "       nan"),
        ("printf '%s|%*s|%5s' a 3 b c", "a|  b|    c"),
    ] {
        let result = run_bash(&tools, command);
        assert_eq!(result["ok"], true, "{command}: {result}");
        assert_eq!(result["exit_code"], 0, "{command}: {result}");
        assert_eq!(result["output"], expected, "{command}: {result}");
    }
}

#[test]
fn embedded_shell_pid_parameters_are_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let dollar = run_bash(&tools, "printf '%s' \"$$\"");
    assert_eq!(dollar["ok"], false, "{dollar}");
    assert_eq!(dollar["exit_code"], 1, "{dollar}");
    let output = dollar["output"].as_str().unwrap();
    assert!(
        output.contains("Independent shell PID is unavailable in embedded execution"),
        "{dollar}"
    );
    assert!(
        !output.contains(&std::process::id().to_string()),
        "embedded shell must not report the host pid: {dollar}"
    );

    let variables = run_bash(
        &tools,
        "printf 'bashpid=[%s] ppid=[%s]' \"$BASHPID\" \"$PPID\"",
    );
    assert_eq!(variables["ok"], true, "{variables}");
    assert_eq!(variables["output"], "bashpid=[] ppid=[]", "{variables}");
}

#[test]
fn embedded_shell_external_background_pid_and_wait() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(
        &tools,
        r#"sh -c 'printf "%s" "$$" > sh.pid; exit 5' &
p=$!
wait "$p"
printf 'waited=%s pid=%s' "$?" "$p"
wait 2147483647
printf ' unknown=%s' "$?"
"#,
    );
    assert_eq!(result["ok"], true, "{result}");
    let output = result["output"].as_str().unwrap();

    // The external shell reported its real OS pid, and `$!` was that same pid.
    let pid = fs::read_to_string(dir.path().join("sh.pid")).unwrap();
    let pid = pid.trim();
    assert!(pid.parse::<u32>().is_ok(), "{result}");
    assert_ne!(pid, std::process::id().to_string(), "{result}");
    assert!(output.contains(&format!("waited=5 pid={pid}")), "{result}");
    // Unknown pids are not claimed as children and report bash's 127.
    assert!(output.contains(" unknown=127"), "{result}");
    assert!(output.contains("not a child of this shell"), "{result}");
}

#[test]
fn embedded_shell_virtual_umask_controls_created_files() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("existing.txt");
    fs::write(&existing, "old").unwrap();
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o666)).unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(
        &tools,
        "umask 077; printf tight > tight.txt; \
         umask 000; printf loose > loose.txt; \
         umask 077; printf replaced > existing.txt",
    );
    assert_eq!(result["ok"], true, "{result}");

    assert_eq!(file_mode(&dir.path().join("tight.txt")), 0o600);
    // The host umask must not tighten a shell that requested 000.
    assert_eq!(file_mode(&dir.path().join("loose.txt")), 0o666);
    // Truncating an existing file never re-chmods it.
    assert_eq!(file_mode(&existing), 0o666);
    assert_eq!(fs::read_to_string(&existing).unwrap(), "replaced");
}

#[test]
fn embedded_shell_exec_argv0_empty_env_and_login() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let argv0 = run_bash(&tools, "exec -a custom sh -c 'printf \"%s\" \"$0\"'");
    assert_eq!(argv0["ok"], true, "{argv0}");
    assert_eq!(argv0["output"], "custom", "{argv0}");

    let empty_env = run_bash(
        &tools,
        "exec -c sh -c 'printf \"home=[%s]\" \"${HOME-<unset>}\"'",
    );
    assert_eq!(empty_env["ok"], true, "{empty_env}");
    assert_eq!(empty_env["output"], "home=[<unset>]", "{empty_env}");

    let login = run_bash(&tools, "exec -l sh -c 'printf \"%s\" \"$0\"'");
    assert_eq!(login["ok"], true, "{login}");
    assert_eq!(login["output"], "-sh", "{login}");
}

#[test]
fn embedded_shell_exec_skips_exit_trap_and_following_statements() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(
        &tools,
        "trap 'printf EXIT-TRAP' EXIT; exec sh -c 'printf exec-target'; printf after",
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["output"], "exec-target", "{result}");
}

#[test]
fn embedded_shell_normal_exit_runs_exit_trap() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(&tools, "trap 'printf EXIT-TRAP' EXIT; printf body; exit 5");
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["exit_code"], 5, "{result}");
    assert_eq!(result["output"], "bodyEXIT-TRAP", "{result}");
}

#[test]
fn embedded_shell_exit_trap_builtin_loop_stops_on_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    // An EXIT handler runs with the run's managed parameters, so its interpreter
    // loop must reach cancellation boundaries instead of outliving the deadline.
    tools.limits.bash_timeout_seconds = 1;
    let started = std::time::Instant::now();
    let result = run_bash(
        &tools,
        "trap 'printf entered; while :; do :; done' EXIT; true",
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "EXIT trap ignored the managed deadline: {elapsed:?}"
    );
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["timed_out"], true, "{result}");
    assert_eq!(result["cancelled"], false, "{result}");
    assert!(
        result["output"].as_str().unwrap().contains("entered"),
        "{result}"
    );
}

#[test]
fn embedded_shell_exit_trap_pipeline_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(
        &tools,
        "trap '{ printf \"hidden\\nvisible\\n\"; exit 7; } | tail -n 1' EXIT; true",
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["output"], "visible\n", "{result}");

    let pipes = &result["pipes"];
    let list = pipes["pipelines"]
        .as_array()
        .unwrap_or_else(|| panic!("missing pipeline list: {result}"));
    assert_eq!(list.len(), 1, "{result}");
    assert_eq!(list[0]["stage_count"], 2, "{result}");
    assert_eq!(list[0]["stages"][0]["status"], 7, "{result}");
    assert_eq!(list[0]["stages"][1]["status"], 0, "{result}");
    assert_eq!(pipes["nonzero_stages"], 1, "{result}");

    let streams = pipes["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 1, "{result}");
    let stream = streams[0]["id"].as_u64().unwrap();
    assert_eq!(streams[0]["open"], false, "{result}");
    let run_id = pipes["run_id"].as_str().unwrap();
    let read = tools
        .pipe_store()
        .read(run_id, stream, None)
        .expect("reading the EXIT trap pipeline stream");
    assert_eq!(read.data, b"hidden\nvisible\n");
}

#[test]
fn embedded_shell_failed_exec_default_and_execfail() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let failed = run_bash(&tools, "exec /nonexistent/pp-embedded-exec; printf after");
    assert_eq!(failed["ok"], false, "{failed}");
    assert_eq!(failed["exit_code"], 127, "{failed}");
    let output = failed["output"].as_str().unwrap();
    assert!(!output.contains("after"), "{failed}");
    assert!(output.contains("command not found"), "{failed}");

    let continued = run_bash(
        &tools,
        "shopt -s execfail; exec /nonexistent/pp-embedded-exec; printf after",
    );
    assert_eq!(continued["ok"], true, "{continued}");
    assert_eq!(continued["exit_code"], 0, "{continued}");
    assert!(
        continued["output"].as_str().unwrap().contains("after"),
        "{continued}"
    );
}

#[test]
fn embedded_shell_subshell_exec_stays_in_subshell() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(&tools, "(exec sh -c 'printf x'; printf bad); printf after");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["output"], "xafter", "{result}");
}

#[test]
fn embedded_shell_logical_descriptors_never_use_host_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(
        &tools,
        r#"exec 3>logical.txt
printf via-fd3 > /dev/fd/3
exec 3>&-
printf via-fd1 > /dev/fd/1
sh -c 'printf child-out; printf child-err >&2'
exec 1>&-
printf hidden > /dev/fd/1
printf ' after-close' >&2
"#,
    );
    assert_eq!(result["ok"], true, "{result}");
    let output = result["output"].as_str().unwrap();
    // `/dev/fd/N` resolves against the logical table, so a cloned fd3 reaches
    // the file and fd1 reaches the captured logical stdout.
    assert!(output.contains("via-fd1"), "{result}");
    assert!(output.contains("child-out"), "{result}");
    assert!(output.contains("child-err"), "{result}");
    assert!(output.contains(" after-close"), "{result}");
    assert!(!output.contains("via-fd3"), "{result}");
    // A closed logical stdout is /dev/null, never the host's stdout.
    assert!(!output.contains("hidden"), "{result}");
    assert_eq!(
        fs::read_to_string(dir.path().join("logical.txt")).unwrap(),
        "via-fd3"
    );
}

#[test]
fn embedded_shell_virtual_ulimit_limits_only_external_children() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(
        &tools,
        "ulimit -f 0; sh -c 'printf x > child-blocked.txt'; \
         printf ' child=%s' \"$?\"; \
         printf parent-ok > parent.txt; \
         ulimit -f > limit.txt",
    );
    assert_eq!(result["ok"], true, "{result}");

    // The external child is constrained by the virtual limit...
    assert_eq!(
        fs::metadata(dir.path().join("child-blocked.txt"))
            .unwrap()
            .len(),
        0
    );
    let output = result["output"].as_str().unwrap();
    let child_status = output
        .split_once(" child=")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .expect("child status");
    assert_ne!(child_status, "0", "{result}");

    // ...while the embedded shell's own builtin redirection is not.
    assert_eq!(
        fs::read_to_string(dir.path().join("parent.txt")).unwrap(),
        "parent-ok"
    );
    // The shell's virtual limit read back exactly what it set; the host process
    // limit was never touched.
    assert_eq!(
        fs::read_to_string(dir.path().join("limit.txt"))
            .unwrap()
            .trim(),
        "0"
    );
}

#[test]
fn embedded_shell_cancellation_stops_pure_builtin_loop() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let token = CancellationToken::new();
    let cancel = token.clone();
    let worker = thread::spawn(move || {
        let tools = Tools::new(root, EditMode::Replace).unwrap();
        let workflow = WorkflowState::default();
        tools
            .execute(
                "bash",
                &json!({"command":"printf ready > ready.marker; while :; do :; done"}).to_string(),
                "embedded-cancel-builtin",
                ToolPolicy::Execute {
                    workflow: &workflow,
                },
                &cancel,
                &mut |_| {},
            )
            .output
    });
    background_marker(&dir.path().join("ready.marker"));
    token.cancel();
    let output = worker.join().expect("cancellation worker");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["cancelled"], true, "{result}");
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["timed_out"], false, "{result}");
}

#[test]
fn embedded_shell_cancellation_reaps_external_child() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let token = CancellationToken::new();
    let cancel = token.clone();
    let worker = thread::spawn(move || {
        let tools = Tools::new(root, EditMode::Replace).unwrap();
        let workflow = WorkflowState::default();
        tools
            .execute(
                "bash",
                &json!({"command":"sh -c 'trap \"\" TERM; printf \"%s\" \"$$\" > child.pid; sleep 60'"}).to_string(),
                "embedded-cancel-child",
                ToolPolicy::Execute {
                    workflow: &workflow,
                },
                &cancel,
                &mut |_| {},
            )
            .output
    });
    let pid = background_marker(&dir.path().join("child.pid"));
    token.cancel();
    let output = worker.join().expect("cancellation worker");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["cancelled"], true, "{result}");
    wait_until_gone(pid.parse().unwrap(), "embedded external child");
}

#[test]
fn embedded_shell_command_substitution_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(&tools, "printf '%s' \"$(printf substituted)\"");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["output"], "substituted", "{result}");
}

#[test]
fn embedded_shell_kill_job_spec_signals_and_reaps() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    // Bound the failure mode where kill does not reach the job: the run reports
    // a timeout instead of sleeping for the full 30 seconds.
    tools.limits.bash_timeout_seconds = 5;
    let result = run_bash(
        &tools,
        "sleep 30 & p=$!; printf '%s' \"$p\" > sleep.pid; kill %1; \
         wait \"$p\"; printf 'status=%s' \"$?\"",
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["timed_out"], false, "{result}");
    assert_eq!(result["cancelled"], false, "{result}");
    let output = result["output"].as_str().unwrap();
    let status = output
        .split_once("status=")
        .map(|(_, rest)| rest.trim())
        .expect("wait status");
    let status: i32 = status.parse().unwrap();
    assert!(status >= 128, "job was not signalled: {result}");
    let pid = fs::read_to_string(dir.path().join("sleep.pid")).unwrap();
    wait_until_gone(pid.trim().parse().unwrap(), "killed background job");
}

#[test]
fn embedded_shell_pipeline_tail_exec_stays_in_stage() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(&tools, "printf x | exec cat; printf after");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["output"], "xafter", "{result}");
}

#[test]
fn embedded_shell_pipeline_tail_function_exec_stays_in_stage() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = run_bash(&tools, "run() { exec cat; }; printf x | run; printf after");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["output"], "xafter", "{result}");
}
