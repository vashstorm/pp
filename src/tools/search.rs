use anyhow::{Result, bail};
use async_openai::types::chat::FunctionObject;
use serde_json::json;
use std::{
    ffi::OsStr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) struct SearchTools {
    find: Option<SearchCommand>,
}
struct SearchCommand {
    name: &'static str,
    executable: PathBuf,
}
impl SearchTools {
    pub(super) fn detect(cwd: &Path, path: Option<&OsStr>) -> Self {
        let find = ["fd", "find"].into_iter().find_map(|name| {
            find_executable(name, cwd, path).map(|executable| SearchCommand { name, executable })
        });
        Self { find }
    }
    pub(super) fn definitions(&self) -> [FunctionObject; 1] {
        let backend = match self.find.as_ref().map(|c| c.name) {
            Some("fd") => {
                r#"Uses fd. Example args: ["--color","never","--type","f","PATTERN","src"]. Pattern precedes directories and is a regex by default; --glob selects glob syntax. By default respects ignore files and skips hidden files; use --hidden / --no-ignore to expand scope."#
            }
            Some(_) => {
                r#"Uses system find (fd is unavailable). Example args: ["src","-type","f","-name","*.rs","-print"]. Paths precede expressions; -name takes a glob in one argv element, without shell glob expansion. Do not use fd's --type / --glob. Does not automatically respect gitignore."#
            }
            None => "Unavailable: find requires fd or find on PATH.",
        };
        [FunctionObject {
            name: "find".into(),
            description: Some(format!(
                "{backend} Pass only native arguments in args, not the program name. No shell expansion or pipelines. Runs in startup cwd with configured bash_timeout_seconds; stdout and stderr retain at most 500 lines combined and 65536 bytes each. No filesystem sandbox; native arguments may have side effects."
            )),
            parameters: Some(
                json!({"type":"object","properties":{"args":{"type":"array","items":{"type":"string"}}},"required":["args"],"additionalProperties":false}),
            ),
            ..Default::default()
        }]
    }
    pub(super) fn command(&self, tool: &str) -> Result<Command> {
        if tool != "find" {
            bail!("unknown search tool: {tool}");
        }
        match &self.find {
            Some(command) => Ok(Command::new(&command.executable)),
            None => bail!("find requires fd or find on PATH"),
        }
    }
}
fn find_executable(name: &str, cwd: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?).find_map(|entry| {
        let candidate = cwd.join(entry).join(name);
        let metadata = candidate.metadata().ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(candidate)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn search_tools_prefer_fd_and_fall_back_to_find() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["fd", "find"] {
            let path = dir.path().join(name);
            fs::write(&path, "").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let tools = SearchTools::detect(dir.path(), Some(dir.path().as_os_str()));
        assert_eq!(
            tools.command("find").unwrap().get_program(),
            dir.path().join("fd")
        );
        fs::remove_file(dir.path().join("fd")).unwrap();
        let tools = SearchTools::detect(dir.path(), Some(dir.path().as_os_str()));
        assert_eq!(
            tools.command("find").unwrap().get_program(),
            dir.path().join("find")
        );
    }
}
