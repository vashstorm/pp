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
    grep: Option<SearchCommand>,
    find: Option<SearchCommand>,
}

struct SearchCommand {
    name: &'static str,
    executable: PathBuf,
}

impl SearchTools {
    pub(super) fn detect(cwd: &Path, path: Option<&OsStr>) -> Self {
        let select = |names: [&'static str; 2]| {
            names.into_iter().find_map(|name| {
                find_executable(name, cwd, path)
                    .map(|executable| SearchCommand { name, executable })
            })
        };
        Self {
            grep: select(["rg", "grep"]),
            find: select(["fd", "find"]),
        }
    }

    pub(super) fn definitions(&self) -> [FunctionObject; 2] {
        let grep = match self.grep.as_ref().map(|command| command.name) {
            Some("rg") => {
                r#"Uses ripgrep (rg). Example args: ["-n","--color","never","-e","PATTERN","src"]. Supports -g globs. By default respects ignore files and skips hidden files; use --hidden / --no-ignore to expand scope. Exit 1 means no matches."#
            }
            Some(_) => {
                r#"Uses system grep (rg is unavailable). Example args: ["-R","-n","-E","-e","PATTERN","--","src"]. Use -R for recursion and -E for extended regex; do not use rg's -g / --hidden. Does not automatically respect gitignore. Exit 1 means no matches."#
            }
            None => "Unavailable: grep requires rg or grep on PATH.",
        };
        let find = match self.find.as_ref().map(|command| command.name) {
            Some("fd") => {
                r#"Uses fd. Example args: ["--color","never","--type","f","PATTERN","src"]. Pattern precedes directories and is a regex by default; --glob selects glob syntax. By default respects ignore files and skips hidden files; use --hidden / --no-ignore to expand scope."#
            }
            Some(_) => {
                r#"Uses system find (fd is unavailable). Example args: ["src","-type","f","-name","*.rs","-print"]. Paths precede expressions; -name takes a glob in one argv element, without shell glob expansion. Do not use fd's --type / --glob. Does not automatically respect gitignore."#
            }
            None => "Unavailable: find requires fd or find on PATH.",
        };
        [("grep", grep), ("find", find)].map(|(name, backend)| FunctionObject {
            name: name.into(),
            description: Some(format!(
                "{backend} Pass only native arguments in args, not the program name. No shell expansion or pipelines. Runs in startup cwd with configured bash_timeout_seconds; stdout and stderr retain at most 500 lines combined and 65536 bytes each. No filesystem sandbox; native arguments may have side effects."
            )),
            parameters: Some(json!({
                "type":"object", "properties":{"args":{"type":"array","items":{"type":"string"}}},
                "required":["args"], "additionalProperties":false
            })),
            ..Default::default()
        })
    }

    pub(super) fn command(&self, tool: &str) -> Result<Command> {
        let (selected, required) = match tool {
            "grep" => (&self.grep, "rg or grep"),
            "find" => (&self.find, "fd or find"),
            _ => bail!("unknown search tool: {tool}"),
        };
        match selected {
            Some(command) => Ok(Command::new(&command.executable)),
            None => bail!("{tool} requires {required} on PATH"),
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
    use std::{fs, os::unix::fs::symlink};

    fn executable(path: &Path) {
        fs::write(path, "").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn selected(tools: &SearchTools, tool: &str) -> PathBuf {
        tools.command(tool).unwrap().get_program().into()
    }

    #[test]
    fn search_tools_prefer_fast_backends_across_path_order() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("fallback");
        let fast = dir.path().join("fast");
        fs::create_dir(&fallback).unwrap();
        fs::create_dir(&fast).unwrap();
        for name in ["grep", "find"] {
            executable(&fallback.join(name));
        }
        for name in ["rg", "fd"] {
            executable(&fast.join(name));
        }
        let path = std::env::join_paths([&fallback, &fast]).unwrap();
        let tools = SearchTools::detect(dir.path(), Some(&path));
        assert_eq!(selected(&tools, "grep"), fast.join("rg"));
        assert_eq!(selected(&tools, "find"), fast.join("fd"));
    }

    #[test]
    fn search_tools_skip_non_executable_directory_and_broken_symlink() {
        for invalid in ["permissions", "directory", "symlink"] {
            let dir = tempfile::tempdir().unwrap();
            for (fast, fallback) in [("rg", "grep"), ("fd", "find")] {
                let candidate = dir.path().join(fast);
                match invalid {
                    "permissions" => {
                        fs::write(&candidate, "").unwrap();
                        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644)).unwrap();
                    }
                    "directory" => fs::create_dir(&candidate).unwrap(),
                    _ => symlink(dir.path().join("missing"), &candidate).unwrap(),
                }
                executable(&dir.path().join(fallback));
            }
            let tools = SearchTools::detect(dir.path(), Some(dir.path().as_os_str()));
            assert_eq!(selected(&tools, "grep"), dir.path().join("grep"));
            assert_eq!(selected(&tools, "find"), dir.path().join("find"));
        }
    }

    #[test]
    fn search_tools_resolve_relative_empty_entries_and_executable_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        executable(&dir.path().join("target"));
        symlink("../target", dir.path().join("bin/rg")).unwrap();
        executable(&dir.path().join("fd"));
        let tools = SearchTools::detect(dir.path(), Some(OsStr::new("missing:bin:")));
        assert_eq!(selected(&tools, "grep"), dir.path().join("bin/rg"));
        assert_eq!(selected(&tools, "find"), dir.path().join("fd"));
    }

    #[test]
    fn search_tools_missing_path_and_candidates_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        for path in [None, Some(dir.path().as_os_str())] {
            let tools = SearchTools::detect(dir.path(), path);
            assert!(tools.command("grep").is_err());
            assert!(tools.command("find").is_err());
        }
    }
}
