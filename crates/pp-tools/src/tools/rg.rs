//! Local content search built on the ripgrep core (grep-searcher/grep-regex/grep-pcre2) and ignore traversal;
//! behaviour follows Oh My Pi d4279540 (MIT License).
use super::{hashline, save};
use anyhow::{Context, Result, ensure};
use pp_config::config::EditMode;
use pp_config::config::RgSettings;
use pp_core::cancellation::CancellationToken;
use serde_json::{Value, json};
#[cfg(test)]
use std::fs;
use std::{
    path::Path,
    time::{Duration, Instant},
};

mod args;
mod budget;
mod paths;
mod pattern;
mod render;
mod scan;

pub(super) use args::definition;
use args::{Args, Many, OutputMode, count, file_types};
use budget::Budget;
pub(in crate::tools) use paths::inspect_roots;
use paths::{TargetIter, parse_paths};
use pattern::{Compiled, compile};
use render::grouped_header;
use scan::{build_searcher, check, run_file};

const FILE_LIMIT: usize = 20;
const MULTI_LIMIT: usize = 20;
const SINGLE_LIMIT: usize = 200;
const TOTAL_LIMIT: usize = 2000;
const MAX_FILE: u64 = 4 * 1024 * 1024;
const MAX_OUTPUT: usize = 50 * 1024;
const MAX_LINE: usize = 512;

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    cwd: &Path,
    settings: RgSettings,
    mode: EditMode,
    workspace: &save::Workspace,
    snapshots: &mut hashline::Store,
    arguments: &str,
    token: &CancellationToken,
    artifacts: Option<&pp_workspace::output::Store>,
) -> Result<Value> {
    run_with_sandbox(
        cwd,
        settings,
        mode,
        workspace,
        snapshots,
        arguments,
        token,
        artifacts,
        pp_sandbox::Sandbox::disabled().view(None),
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_with_sandbox(
    cwd: &Path,
    settings: RgSettings,
    mode: EditMode,
    workspace: &save::Workspace,
    snapshots: &mut hashline::Store,
    arguments: &str,
    token: &CancellationToken,
    artifacts: Option<&pp_workspace::output::Store>,
    sandbox: pp_sandbox::SandboxView<'_>,
) -> Result<Value> {
    let policy = sandbox.policy()?;
    run_with_policy(
        cwd, settings, mode, workspace, snapshots, arguments, token, artifacts, policy,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::tools) fn run_with_policy(
    cwd: &Path,
    settings: RgSettings,
    mode: EditMode,
    workspace: &save::Workspace,
    snapshots: &mut hashline::Store,
    arguments: &str,
    token: &CancellationToken,
    artifacts: Option<&pp_workspace::output::Store>,
    policy: Option<std::sync::Arc<pp_sandbox::Policy>>,
) -> Result<Value> {
    let mut args = Args::parse(arguments)?;
    let mut budget = Budget::new(args.max_output_tokens)?;
    let settings = args.settings(settings)?;
    let insensitive = args.insensitive()?;
    let output_mode = args.output_mode;
    let head_limit = Some(count(args.head_limit, "head_limit")?).filter(|n| *n > 0);
    let ignored = std::mem::take(&mut args.ignored);
    if args.path.as_deref().is_some_and(super::read::is_pipe_uri) {
        anyhow::bail!("Temporary pipe streams support read only; use read with a pipe:// URI.");
    }
    let uri = args
        .path
        .as_deref()
        .filter(|p| p.starts_with("artifact://"))
        .map(str::to_owned);
    let artifact = uri
        .as_ref()
        .map(|uri| {
            artifacts
                .context("no session artifacts available")?
                .snapshot(uri, token)
        })
        .transpose()?;
    if let Some(snapshot) = &artifact {
        args.path = Some(snapshot.path.to_string_lossy().into_owned());
    }
    ensure!(!args.pattern.trim().is_empty(), "Pattern must not be empty");
    let skip = count(args.skip, "Skip")?;
    let max_count = args
        .max_count
        .map(|value| count(Some(value), "max_count"))
        .transpose()?;
    let fixed = args.fixed.unwrap_or(false);
    let word = args.word.unwrap_or(false);
    let types = file_types(args.types.take())?;
    let globs: Vec<String> = args
        .globs
        .take()
        .into_iter()
        .chain(args.glob.take())
        .flat_map(Many::into_vec)
        .collect();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (targets, single, missing) = if max_count == Some(0) {
        (TargetIter::List(Vec::new().into_iter()), false, Vec::new())
    } else {
        parse_paths(
            cwd,
            args.path.as_deref().unwrap_or("."),
            args.gitignore.unwrap_or(true),
            &globs,
            types.as_ref(),
            token,
            deadline,
            artifact.is_none().then_some(policy).flatten(),
        )?
    };
    let multiline = args.pattern.contains('\n') || (!fixed && args.pattern.contains("\\n"));
    let compiled = compile(&args.pattern, insensitive, multiline, fixed, word)?;
    let per_file = if single { SINGLE_LIMIT } else { MULTI_LIMIT };
    let mut found = Vec::new();
    let mut total = 0;
    let mut internal_limit = false;
    let mut warnings = Vec::new();
    let mut page_searcher = None;
    let mut skip_searcher = None;
    let mut buffer = Vec::new();
    for target in targets {
        let mut target = target?;
        if let Some(uri) = &uri {
            target.display = uri.clone();
        }
        check(token, deadline)?;
        let in_page =
            single || (found.len() >= skip && found.len() < skip.saturating_add(FILE_LIMIT));
        let collect_rows = output_mode == OutputMode::Content && in_page;
        let capture_snapshot = collect_rows && mode == EditMode::Hashline && artifact.is_none();
        let searcher = if collect_rows {
            page_searcher.get_or_insert_with(|| {
                build_searcher(multiline, settings.context_before, settings.context_after)
            })
        } else {
            skip_searcher.get_or_insert_with(|| build_searcher(multiline, 0, 0))
        };
        let result = match &compiled {
            Compiled::Rust(m) => run_file(
                m,
                searcher,
                &mut buffer,
                target,
                settings.context_before,
                settings.context_after,
                multiline,
                per_file,
                max_count,
                output_mode,
                collect_rows,
                capture_snapshot,
                token,
                deadline,
            ),
            Compiled::Pcre(m) => run_file(
                m,
                searcher,
                &mut buffer,
                target,
                settings.context_before,
                settings.context_after,
                multiline,
                per_file,
                max_count,
                output_mode,
                collect_rows,
                capture_snapshot,
                token,
                deadline,
            ),
        };
        match result {
            Ok((matched, large_warning)) => {
                if let Some(warning) = large_warning {
                    warnings.push(warning);
                }
                if let Some(f) = matched {
                    total += f.matches;
                    found.push(f);
                    if output_mode == OutputMode::Content && total >= TOTAL_LIMIT {
                        internal_limit = true;
                        break;
                    }
                }
            }
            Err(e) => {
                check(token, deadline)?;
                warnings.push(format!("Skipped file: {e:#}"));
            }
        }
    }
    let count = found.len();
    let start = if single { 0 } else { skip.min(count) };
    let end = if single {
        count
    } else {
        start.saturating_add(FILE_LIMIT).min(count)
    };
    let page = &found[start..end];
    let file_limit = end < count;
    let mut content = String::new();
    let mut visible_matches = 0;
    let mut line_cut = false;
    let mut byte_cut = false;
    let mut file_details = Vec::new();
    let mut common = page
        .first()
        .map(|file| {
            file.target
                .display
                .split('/')
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    common.pop();
    for file in page.iter().skip(1) {
        let mut dirs = file
            .target
            .display
            .split('/')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();
        dirs.pop();
        common.truncate(
            common
                .iter()
                .zip(dirs.iter())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    let mut previous_dirs = Vec::new();
    // `head_limit`: content mode counts body lines, other modes count entries.
    let mut emitted = 0usize;
    let mut head_cut = false;
    for file in page {
        if head_limit.is_some_and(|limit| emitted >= limit) {
            head_cut = true;
            break;
        }
        if output_mode != OutputMode::Content {
            let text = if output_mode == OutputMode::Count {
                format!("{}:{}\n", file.target.display, file.matches)
            } else {
                format!("{}\n", file.target.display)
            };
            if content.len() + text.len() > MAX_OUTPUT - 2048 {
                byte_cut = true;
                break;
            }
            if !budget.allows(&content, &text)? {
                break;
            }
            content.push_str(&text);
            emitted += 1;
            if output_mode == OutputMode::Count {
                visible_matches += file.matches;
                file_details.push(json!({"path":file.target.display,"count":file.matches}));
            } else {
                file_details.push(json!({"path":file.target.display}));
            }
            continue;
        }
        let key = workspace.key(&file.target.path).ok();
        let editable = if mode == EditMode::Hashline {
            match (&key, &file.bytes) {
                (Some(k), Some(raw))
                    if !k.to_string_lossy().contains(['[', ']', '\n', '\r'])
                        && !raw.contains('\0')
                        && !raw
                            .as_bytes()
                            .windows(2)
                            .any(|pair| pair[0] == b'\r' && pair[1] != b'\n')
                        && !raw.ends_with('\r') =>
                {
                    // The search already read the body; this re-reads it once to prove the snapshot matches the current disk bytes,
                    // and the tag is computed on that same comparison instead of being recomputed by `record`.
                    workspace
                        .target(&file.target.path, false)
                        .ok()
                        .and_then(|t| t.read_bounded(hashline::MAX_FILE_BYTES).ok())
                        .filter(|b| b.as_slice() == raw.as_bytes())
                        .map(|_| (k.clone(), raw.clone(), hashline::file_hash(raw)))
                }
                _ => None,
            }
        } else {
            None
        };
        // A single write completes the registration; `extend_seen` adds the actually displayed lines after rendering.
        let editable = editable
            .filter(|(key, raw, tag)| snapshots.record(key.clone(), tag, raw.clone(), Vec::new()));
        let mut header = if single {
            editable
                .as_ref()
                .map(|(k, _, tag)| format!("[{}#{tag}]\n", k.display()))
                .unwrap_or_default()
        } else {
            grouped_header(
                &file.target.display,
                editable.as_ref().map(|(_, _, t)| t.as_str()),
                common.len(),
                &mut previous_dirs,
            )
        };
        if !content.is_empty() {
            header.insert(0, '\n');
        }
        if content.len() + header.len() > MAX_OUTPUT - 2048 {
            byte_cut = true;
            break;
        }
        if !budget.allows(&content, &header)? {
            break;
        }
        content.push_str(&header);
        let mut seen = Vec::new();
        let mut last = None;
        for row in &file.rows {
            if head_limit.is_some_and(|limit| emitted >= limit) {
                head_cut = true;
                break;
            }
            let sep = if last.is_some_and(|n| row.number > n + 1) {
                "...\n"
            } else {
                ""
            };
            let marker = if row.matched { '*' } else { ' ' };
            let delimiter = if editable.is_some() { ':' } else { '|' };
            let column = row
                .column
                .map(|n| format!(" [col {n}]"))
                .unwrap_or_default();
            let text = format!(
                "{sep}{marker}{}{delimiter}{}{column}\n",
                row.number, row.text
            );
            if content.len() + text.len() > MAX_OUTPUT - 2048 {
                byte_cut = true;
                break;
            }
            if !budget.allows(&content, &text)? {
                break;
            }
            content.push_str(&text);
            emitted += 1;
            if !row.truncated {
                seen.push(row.number..row.number + 1);
            }
            if row.matched {
                visible_matches += 1;
            }
            line_cut |= row.truncated;
            last = Some(row.number);
        }
        if let Some((key, _, _)) = &editable {
            snapshots.extend_seen(key, seen);
        }
        file_details.push(json!({"path":file.target.display,"count":file.matches}));
        if byte_cut || budget.reached || head_cut {
            break;
        }
    }
    let count_complete = warnings.is_empty() && missing.is_empty();
    let mut notes = Vec::new();
    if !ignored.is_empty() {
        notes.push(format!(
            "Ignored unsupported arguments: {}.",
            ignored.join(", ")
        ));
    }
    // On head_limit truncation no whole-page skip is emitted, so it never suggests skipping files that were not shown.
    let entry_head_cut = head_cut && output_mode != OutputMode::Content;
    if file_limit && !head_cut {
        notes.push(format!(
            "Showing files {}-{} of {}{}. Use skip={} for the next page, or narrow paths/pattern.",
            start + 1,
            end,
            count,
            if internal_limit { "+" } else { "" },
            end
        ));
    }
    if internal_limit {
        notes.push("Internal search match limit reached; narrow paths or pattern.".into());
    }
    if let Some(limit) = head_limit.filter(|_| head_cut) {
        notes.push(if entry_head_cut {
            format!(
                "Output capped at head_limit={limit} entries. Use skip={} for more.",
                start + emitted
            )
        } else {
            format!(
                "Output capped at head_limit={limit} lines; raise head_limit or narrow paths/pattern."
            )
        });
    }
    for f in page.iter().filter(|f| f.capped) {
        notes.push(format!(
            "More than {per_file} matches in {}; narrow the pattern or search one file.",
            f.target.display
        ));
    }
    if !missing.is_empty() {
        notes.push(format!("Skipped missing paths: {}", missing.join(", ")));
    }
    if byte_cut {
        notes.push("Output truncated by byte limit; narrow paths or pattern.".into());
    }
    if line_cut {
        notes.push("Some lines were truncated to 512 bytes.".into());
    }
    if output_mode == OutputMode::Count && !count_complete {
        notes.push("Counts are partial: some files or bytes were not searched.".into());
    }
    notes.extend(warnings.into_iter().take(10));
    if content.is_empty() && !budget.reached {
        let text = if skip > 0 && count > 0 {
            "No more results"
        } else {
            "No matches found"
        };
        if budget.allows(&content, text)? {
            content.push_str(text);
        }
    }
    if output_mode == OutputMode::Count {
        let text = format!(
            "\nTotal{}: {total}",
            if count_complete { "" } else { " (partial)" }
        );
        if budget.allows(&content, &text)? {
            content.push_str(&text);
        }
    }
    if budget.enabled() {
        for (index, note) in notes.iter().enumerate() {
            let separator = if content.is_empty() {
                ""
            } else if index == 0 {
                "\n\n"
            } else {
                "\n"
            };
            let text = format!("{separator}{note}");
            if content.len() + text.len() > MAX_OUTPUT || !budget.allows(&content, &text)? {
                break;
            }
            content.push_str(&text);
        }
    } else if !notes.is_empty() {
        content.push_str("\n\n");
        let mut notice = notes.join("\n");
        let remaining = MAX_OUTPUT.saturating_sub(content.len());
        if notice.len() > remaining {
            let mut end = remaining.saturating_sub(24).min(notice.len());
            while !notice.is_char_boundary(end) {
                end -= 1;
            }
            notice.truncate(end);
            notice.push_str("\n... notices truncated");
        }
        content.push_str(&notice);
    }
    budget.finish(&mut content)?;
    let mut result = json!({"ok":true,"content":content,"details":{"matchCount":visible_matches,"fileCount":file_details.len(),"fileMatches":file_details,"fileLimitReached":file_limit,"perFileLimitReached":page.iter().any(|f|f.capped),"linesTruncated":line_cut,"truncated":file_limit || internal_limit || byte_cut || line_cut || head_cut || page.iter().any(|f|f.capped),"scopePath":uri.or(args.path).unwrap_or_else(||".".into())}});
    if head_limit.is_some() {
        result["details"]["headLimitReached"] = json!(head_cut);
    }
    if !ignored.is_empty() {
        result["details"]["ignoredArguments"] = json!(ignored);
    }
    if output_mode != OutputMode::Content {
        result["details"]["outputMode"] = json!(output_mode.as_str());
    }
    if output_mode == OutputMode::FilesWithMatches {
        result["details"]
            .as_object_mut()
            .expect("details is an object")
            .remove("matchCount");
    } else if output_mode == OutputMode::Count {
        result["details"]["totalMatchCount"] = json!(total);
        result["details"]["countComplete"] = json!(count_complete);
        result["details"]["truncated"] =
            json!(file_limit || byte_cut || head_cut || !count_complete);
    }
    if budget.enabled() {
        result["details"]["tokenLimitReached"] = json!(budget.reached);
        if budget.reached {
            result["details"]["truncated"] = json!(true);
        }
    }
    if let Some(snapshot) = artifact {
        result["artifact"] = snapshot.metadata;
    }
    Ok(result)
}

#[cfg(test)]
mod compat_tests;

#[cfg(test)]
mod budget_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn execute(dir: &Path, args: Value, mode: EditMode, store: &mut hashline::Store) -> Value {
        let canonical = dir.canonicalize().unwrap();
        run(
            &canonical,
            RgSettings::default(),
            mode,
            &save::Workspace::open(&canonical).unwrap(),
            store,
            &args.to_string(),
            &CancellationToken::new(),
            None,
        )
        .unwrap()
    }
    #[test]
    fn rg_native_regex_pagination_and_hashline_tags() {
        {
            let d = tempfile::tempdir().unwrap();
            fs::create_dir(d.path().join("src")).unwrap();
            fs::write(
                d.path().join("src/a.rs"),
                "first\nfetchProvider(\nfoobar\nfooquux\nlast\n",
            )
            .unwrap();
            fs::write(d.path().join("src/b.txt"), "foobar\n").unwrap();
            fs::write(d.path().join("4"), "unrelated\n").unwrap();
            fs::write(d.path().join("txt}"), "unrelated\n").unwrap();
            fs::create_dir(d.path().join("src/nested")).unwrap();
            fs::write(d.path().join("src/nested/deep.rs"), "foobar\n").unwrap();
            let mut store = hashline::Store::default();
            let a = execute(
                d.path(),
                json!({"pattern":"foo(?=bar)","path":"src/*.rs"}),
                EditMode::Replace,
                &mut store,
            );
            assert!(a["content"].as_str().unwrap().contains("*3|foobar"), "{a}");
            assert!(!a["content"].as_str().unwrap().contains("deep.rs"), "{a}");
            let braces = execute(
                d.path(),
                json!({"pattern":"foobar", "path":"src/*.{rs,txt}"}),
                EditMode::Replace,
                &mut store,
            );
            assert_eq!(braces["details"]["fileCount"], 2, "{braces}");
            let recursive = execute(
                d.path(),
                json!({"pattern":"foobar", "path":"src/**/*.rs"}),
                EditMode::Replace,
                &mut store,
            );
            assert!(
                recursive["content"].as_str().unwrap().contains("deep.rs"),
                "{recursive}"
            );
            let b = execute(
                d.path(),
                json!({"pattern":"fetchProvider(","path":"src/a.rs:2-2"}),
                EditMode::Replace,
                &mut store,
            );
            assert!(
                b["content"].as_str().unwrap().contains("*2|fetchProvider("),
                "{b}"
            );
            assert!(!b["content"].as_str().unwrap().contains("foobar"));
            let onward = execute(
                d.path(),
                json!({"pattern":"foo", "path":"src/a.rs:4"}),
                EditMode::Replace,
                &mut store,
            );
            assert!(
                onward["content"].as_str().unwrap().contains("*4|fooquux"),
                "{onward}"
            );
            assert!(
                !onward["content"].as_str().unwrap().contains("foobar"),
                "{onward}"
            );
            let ranged = execute(
                d.path(),
                json!({"pattern":"foo", "path":"src/a.rs:L3..L4"}),
                EditMode::Replace,
                &mut store,
            );
            assert_eq!(ranged["details"]["matchCount"], 2, "{ranged}");
            let disjoint = execute(
                d.path(),
                json!({"pattern":"foo", "path":"src/a.rs:3,4"}),
                EditMode::Replace,
                &mut store,
            );
            assert_eq!(disjoint["details"]["matchCount"], 2, "{disjoint}");
            let c = execute(
                d.path(),
                json!({"pattern":"foo\\nbar", "path":"src/a.rs"}),
                EditMode::Replace,
                &mut store,
            );
            assert_eq!(c["ok"], true);
        }

        {
            let d = tempfile::tempdir().unwrap();
            for i in 0..22 {
                fs::write(d.path().join(format!("{i:02}.txt")), "needle\nother\n").unwrap();
            }
            let mut store = hashline::Store::default();
            let page = execute(
                d.path(),
                json!({"pattern":"needle"}),
                EditMode::Replace,
                &mut store,
            );
            assert_eq!(page["details"]["fileCount"], 20, "{page}");
            assert!(page["content"].as_str().unwrap().contains("skip=20"));
            let second = execute(
                d.path(),
                json!({"pattern":"needle","skip":20}),
                EditMode::Replace,
                &mut store,
            );
            assert_eq!(second["details"]["fileCount"], 2);
            let file = execute(
                d.path(),
                json!({"pattern":"needle","path":"00.txt"}),
                EditMode::Hashline,
                &mut store,
            );
            assert!(
                file["content"].as_str().unwrap().contains("[00.txt#"),
                "{file}"
            );
        }

        {
            let d = tempfile::tempdir().unwrap();
            fs::write(d.path().join("file"), "hello\n").unwrap();
            let mut store = hashline::Store::default();
            hashline::take_tag_computations();
            let result = execute(
                d.path(),
                json!({"pattern":"hello","path":"file"}),
                EditMode::Hashline,
                &mut store,
            );
            // Upstream vector: file_hash("hello\n") == 5BF9. The tag must come from the disk bytes and be computed once per write.
            assert!(
                result["content"]
                    .as_str()
                    .unwrap()
                    .starts_with("[file#5BF9]\n"),
                "{result}"
            );
            assert_eq!(hashline::take_tag_computations(), 1, "{result}");
            // The snapshot body equals the current disk bytes: shown lines are editable, hidden ones are not.
            let shown = hashline::parse("[file#5BF9]\nCUT 1.=1").unwrap();
            let hidden = hashline::parse("[file#5BF9]\nCUT 2.=2").unwrap();
            assert!(
                store
                    .validate(Path::new("file"), "5BF9", "hello\n", &shown.edits)
                    .is_ok()
            );
            assert!(
                store
                    .validate(Path::new("file"), "5BF9", "hello\n", &hidden.edits)
                    .is_err()
            );
        }
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    fn query(cwd: &Path, args: Value) -> Value {
        let cwd = cwd.canonicalize().unwrap();
        run(
            &cwd,
            RgSettings::default(),
            EditMode::Replace,
            &save::Workspace::open(&cwd).unwrap(),
            &mut hashline::Store::default(),
            &args.to_string(),
            &CancellationToken::new(),
            None,
        )
        .unwrap()
    }
    #[test]
    fn rg_walk_boundaries_multiline_symlinks_and_large_files() {
        {
            let d = tempfile::tempdir().unwrap();
            fs::write(d.path().join(".ignore"), "ignored.txt\n").unwrap();
            fs::write(d.path().join("ignored.txt"), "need\n").unwrap();
            fs::write(d.path().join(".hidden"), "need\n").unwrap();
            fs::write(d.path().join("literal[1].txt"), "need\n").unwrap();
            fs::create_dir(d.path().join("vendor")).unwrap();
            fs::write(d.path().join("vendor/a.txt"), "need\n").unwrap();
            let r = query(d.path(), json!({"pattern":"need","path":".;!vendor"}));
            let text = r["content"].as_str().unwrap();
            assert!(text.contains(".hidden"), "{text}");
            assert!(!text.contains("ignored.txt"));
            assert!(!text.contains("vendor"));
            let exact = query(d.path(), json!({"pattern":"need","path":"literal[1].txt"}));
            assert!(exact["content"].as_str().unwrap().contains("*1|need"));
            let no_ignore = query(d.path(), json!({"pattern":"need","gitignore":false}));
            assert!(
                no_ignore["content"]
                    .as_str()
                    .unwrap()
                    .contains("ignored.txt")
            );
        }

        {
            let d = tempfile::tempdir().unwrap();
            fs::write(
                d.path().join("file"),
                format!("foo\nbar\n{}needle-end\n", "中".repeat(1000)),
            )
            .unwrap();
            let cross = query(d.path(), json!({"pattern":"foo\\nbar","path":"file"}));
            assert!(
                cross["content"].as_str().unwrap().contains("*1|foo"),
                "{cross}"
            );
            let long = query(d.path(), json!({"pattern":"needle","path":"file"}));
            let text = long["content"].as_str().unwrap();
            assert!(
                text.contains("needle-end") && text.contains("[col 1001]"),
                "{text}"
            );
            assert_eq!(long["details"]["linesTruncated"], true);
        }

        {
            use std::os::unix::fs::symlink;
            let d = tempfile::tempdir().unwrap();
            fs::write(d.path().join("target"), "needle").unwrap();
            symlink("target", d.path().join("link")).unwrap();
            let cwd = d.path().canonicalize().unwrap();
            let workspace = save::Workspace::open(&cwd).unwrap();
            let call = |path: &str, token: &CancellationToken| {
                run(
                    &cwd,
                    RgSettings::default(),
                    EditMode::Replace,
                    &workspace,
                    &mut hashline::Store::default(),
                    &json!({"pattern":"needle","path":path}).to_string(),
                    token,
                    None,
                )
            };
            assert!(call("link", &CancellationToken::new()).is_err());
            let t = CancellationToken::new();
            t.cancel();
            assert!(
                call("target", &t)
                    .unwrap_err()
                    .downcast_ref::<pp_core::cancellation::Cancelled>()
                    .is_some()
            );
        }

        {
            let d = tempfile::tempdir().unwrap();
            let mut content = "padding\n".repeat(524_289);
            content.push_str("needle-at-end\n");
            fs::write(d.path().join("large.txt"), content).unwrap();
            let direct = query(
                d.path(),
                json!({"pattern":"needle-at-end", "path":"large.txt"}),
            );
            assert!(
                direct["content"]
                    .as_str()
                    .unwrap()
                    .contains("needle-at-end"),
                "{direct}"
            );
            let directory = query(d.path(), json!({"pattern":"needle-at-end"}));
            let text = directory["content"].as_str().unwrap();
            assert!(
                text.contains("No matches found") && text.contains("first 4 MiB"),
                "{text}"
            );
        }
    }
}

#[cfg(test)]
mod pipe_stream_tests {
    use super::*;

    #[test]
    fn pipeline_rg_rejects_pipe_streams_with_read_only_message() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let workspace = save::Workspace::open(&cwd).unwrap();
        let error = run(
            &cwd,
            RgSettings::default(),
            EditMode::Replace,
            &workspace,
            &mut hashline::Store::default(),
            &json!({"pattern": "x", "path": "pipe://abc/1"}).to_string(),
            &CancellationToken::new(),
            None,
        )
        .err()
        .unwrap();
        assert_eq!(
            error.to_string(),
            "Temporary pipe streams support read only; use read with a pipe:// URI."
        );
    }
}
