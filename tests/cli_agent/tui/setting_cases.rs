use super::*;
use pp_config::config::{EditMode, IconMode, Theme, read_settings};

#[test]
fn tui_setting_tabs_persist_general_and_revert_unsaved_theme() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, home.path());
    pty.ready();
    let settings_path = config_path(home.path(), "settings.yml");

    // 1. `/setting` opens General, switches `edit: replace -> hashline` and persists it to settings.yml.
    pty.input("/setting\r");
    pty.text("General —");
    pty.text("Editing & Tools");
    pty.text("edit");
    pty.input(" ");
    pty.wait_for("edit switches to hashline and is written to disk", |_| {
        read_settings(&settings_path).is_ok_and(|settings| settings.edit == EditMode::Hashline)
    });

    // 2. Press `→` to move through Roles and Themes and preview ASCII (unsaved); after `←` returns to General the theme preview rolls back.
    pty.input("\x1b[C");
    pty.text("Model roles");
    pty.input("\x1b[C");
    pty.text("Themes — Icon Style");
    pty.input("\x1b[B");
    pty.text("ASCII");
    pty.input("\x1b[D");
    pty.text("Model roles");
    pty.input("\x1b[D");
    pty.text("General —");
    let saved = read_settings(&settings_path).unwrap();
    assert_eq!(
        (saved.tui.theme, saved.tui.icons),
        (Theme::Default, IconMode::Nerd)
    );

    // 3. Press `←` to cycle to Skills, `Esc` to close the unified Setting panel, then use the `/config` alias to verify the config survived a restart.
    pty.input("\x1b[D");
    pty.text("Skills —");
    pty.input("\x1b");
    pty.ready();
    pty.input("\x03");
    pty.finish();

    let mut restarted = Pty::start_with_existing_files(home.path());
    restarted.ready();
    restarted.input("/config\r");
    restarted.text("General —");
    restarted.text("hashline");
    restarted.input("\x1b");
    restarted.ready();
    restarted.input("\x03");
    restarted.finish();
    no_more(&server);
}
