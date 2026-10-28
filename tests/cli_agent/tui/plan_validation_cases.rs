use super::*;

#[test]
fn plan_tui_external_file_errors_never_restore_or_execute_snapshot_bytes() {
    for protocol in ["completion", "response"] {
        for fault in [
            "missing",
            "symlink",
            "directory",
            "invalid_utf8",
            "oversized",
        ] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
            plan_enter(&mut pty, &fixture);
            plan_drive_ready(
                &mut pty,
                &fixture,
                protocol,
                "Plan before external file error",
                PLAN_TUI_BODY,
            );
            let path = plan_path(project.path());
            match fault {
                "missing" => std::fs::remove_file(&path).unwrap(),
                "symlink" => {
                    std::fs::write(project.path().join("outside-plan"), PLAN_TUI_BODY).unwrap();
                    std::fs::remove_file(&path).unwrap();
                    std::os::unix::fs::symlink(project.path().join("outside-plan"), &path).unwrap();
                }
                "directory" => {
                    std::fs::remove_file(&path).unwrap();
                    std::fs::create_dir(&path).unwrap();
                }
                "invalid_utf8" => std::fs::write(&path, b"\xff\xfe").unwrap(),
                "oversized" => std::fs::write(&path, vec![b'x'; 262_145]).unwrap(),
                _ => unreachable!(),
            }
            plan_choose(&mut pty, 1);
            pty.wait_for("file validation failure keeps planning", |screen| {
                let text = cancel_normalize(screen);
                text.contains("Plan")
                    && (text.contains("UTF-8")
                        || text.contains("regularfile")
                        || text.contains("Nosuchfile")
                        || text.contains("toolarge")
                        || text.contains("exceeds"))
            });
            no_more(&fixture);
            assert!(!project.path().join("canary.txt").exists());
            match fault {
                "missing" => assert!(!path.exists()),
                "symlink" => assert!(
                    std::fs::symlink_metadata(&path)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                ),
                "directory" => assert!(path.is_dir()),
                "invalid_utf8" => assert_eq!(std::fs::read(&path).unwrap(), b"\xff\xfe"),
                "oversized" => assert_eq!(std::fs::metadata(&path).unwrap().len(), 262_145),
                _ => unreachable!(),
            }
            pty.input("\x03");
            pty.finish();
        }
    }
}
