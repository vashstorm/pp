use super::*;

#[test]
fn slash_dump_completion_submission_and_worker_result_display() {
    use pp_runtime::agent::{DumpFile, DumpMode, DumpOutput};
    use std::path::PathBuf;

    let (mut app, requests, _events) = plan_app();
    app.state = State::Ready;

    app.input.insert_str("/dump ");
    app.refresh_candidates(true);
    let menu = app.menu.as_ref().expect("dump completion menu");
    let labels: Vec<&str> = menu
        .items()
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert_eq!(labels, vec!["all", "result"]);

    app.handle(key(KeyCode::Enter), &mut false);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::Dump(DumpMode::All))
    ));
    assert!(app.raw_input_text().is_empty());

    let output = DumpOutput {
        mode: DumpMode::All,
        directory: PathBuf::from("/tmp"),
        files: vec![
            DumpFile {
                label: "Main transcript".into(),
                path: PathBuf::from("/tmp/pp-s1-main.jsonl"),
                file_name: "pp-s1-main.jsonl".into(),
            },
            DumpFile {
                label: "LLM request JSON".into(),
                path: PathBuf::from("/tmp/pp-s1-request.json"),
                file_name: "pp-s1-request.json".into(),
            },
        ],
    };
    app.receive_event_for_test(WorkerEvent::Dumped(Ok(output)));
    let rendered = draw(&mut app, 120, 30);
    assert!(rendered.contains("/tmp/pp-s1-main.jsonl"), "{rendered}");
    assert!(rendered.contains("/tmp/pp-s1-request.json"), "{rendered}");
}
