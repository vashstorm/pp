use super::COMMANDS;
use anyhow::{Context, Result, ensure};
use std::{fs, io, path::Path};

#[derive(Default)]
pub struct FileCommands {
    pub entries: Vec<FileCommand>,
    pub diagnostics: Vec<String>,
}

pub struct FileCommand {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) content: String,
}

impl FileCommands {
    pub fn from_dir(agent_dir: &Path) -> Self {
        let mut commands = Self::default();
        let root = agent_dir.join("commands");
        let directory = match fs::read_dir(&root) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Err(missing) = fs::symlink_metadata(&root)
                    && missing.kind() == io::ErrorKind::NotFound
                {
                    return commands;
                }
                commands
                    .diagnostics
                    .push(format!("{}: {error}", root.display()));
                return commands;
            }
            Err(error) => {
                commands
                    .diagnostics
                    .push(format!("{}: {error}", root.display()));
                return commands;
            }
        };
        let mut paths = Vec::new();
        for entry in directory {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => commands
                    .diagnostics
                    .push(format!("{}: {error}", root.display())),
            }
        }
        paths.sort();
        for path in paths {
            if path.extension().is_none_or(|extension| extension != "md")
                || path
                    .file_name()
                    .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
            {
                continue;
            }
            let result = (|| -> Result<Option<FileCommand>> {
                if !fs::metadata(&path)
                    .context("Failed to read command metadata")?
                    .is_file()
                {
                    return Ok(None);
                }
                let stem = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("");
                ensure!(
                    !stem.is_empty() && !stem.chars().any(|c| c.is_whitespace() || c.is_control()),
                    "invalid command name"
                );
                let name = format!("/{stem}");
                ensure!(
                    !matches!(name.as_str(), "/think" | "/models")
                        && !COMMANDS
                            .iter()
                            .any(|spec| spec.name == name || spec.aliases.contains(&name.as_str())),
                    "command name conflicts with a built-in: {name}"
                );
                ensure!(
                    !commands.entries.iter().any(|command| command.name == name),
                    "duplicate command name: {name}"
                );
                let text = fs::read_to_string(&path).context("Failed to read command file")?;
                let (content, description) = metadata(&text)?;
                ensure!(!content.trim().is_empty(), "command body must not be blank");
                let description = description
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_else(|| {
                        let line = content
                            .lines()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("")
                            .trim();
                        let mut chars = line.chars();
                        let mut description: String = chars.by_ref().take(60).collect();
                        if chars.next().is_some() {
                            description.push_str("...");
                        }
                        description
                    });
                Ok(Some(FileCommand {
                    name,
                    description,
                    content: content.to_owned(),
                }))
            })();
            match result {
                Ok(Some(command)) => commands.entries.push(command),
                Ok(None) => {}
                Err(error) => commands
                    .diagnostics
                    .push(format!("{}: {error:#}", path.display())),
            }
        }
        commands.entries.sort_by(|a, b| a.name.cmp(&b.name));
        commands
    }
}

impl FileCommand {
    pub(crate) fn expand(&self, argument: &str) -> String {
        let args = split_arguments(argument);
        let mut output = String::with_capacity(self.content.len() + argument.len());
        let mut remaining = self.content.as_str();
        let mut recognized = false;
        while let Some(index) = remaining.find('$') {
            output.push_str(&remaining[..index]);
            remaining = &remaining[index..];
            let placeholder = if remaining.starts_with("$ARGUMENTS") {
                Some((10, 0, args.len()))
            } else if remaining.starts_with("$@") {
                slice_placeholder(remaining).or(Some((2, 0, args.len())))
            } else {
                let digits = remaining[1..]
                    .bytes()
                    .take_while(u8::is_ascii_digit)
                    .count();
                (digits > 0).then(|| {
                    let position = decimal(&remaining[1..1 + digits]);
                    let start = position.checked_sub(1).unwrap_or(usize::MAX);
                    (1 + digits, start, start.saturating_add(1))
                })
            };
            if let Some((length, start, end)) = placeholder {
                recognized = true;
                if start < args.len() {
                    append_arguments(&mut output, &args[start..end.min(args.len())]);
                }
                remaining = &remaining[length..];
            } else {
                output.push('$');
                remaining = &remaining[1..];
            }
        }
        output.push_str(remaining);
        if !recognized && !args.is_empty() {
            output.truncate(output.trim_end().len());
            output.push_str("\n\n");
            append_arguments(&mut output, &args);
        }
        output
    }
}

fn append_arguments(output: &mut String, args: &[String]) {
    for (index, arg) in args.iter().enumerate() {
        if index != 0 {
            output.push(' ');
        }
        output.push_str(arg);
    }
}

fn split_arguments(argument: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for c in argument.chars() {
        match quote {
            Some(delimiter) if c == delimiter => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == ' ' || c == '\t' => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

// Saturating decimal parsing keeps arbitrarily large indices and lengths harmless.
fn decimal(digits: &str) -> usize {
    digits.bytes().fold(0usize, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(usize::from(digit - b'0'))
    })
}

fn slice_placeholder(text: &str) -> Option<(usize, usize, usize)> {
    let text = text.strip_prefix("$@[")?;
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let position = decimal(&text[..digits]);
    let start = position.checked_sub(1).unwrap_or(usize::MAX);
    let suffix = &text[digits..];
    if suffix.starts_with(']') {
        return Some((3 + digits + 1, start, usize::MAX));
    }
    let suffix = suffix.strip_prefix(':')?;
    let length_digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
    if !suffix[length_digits..].starts_with(']') {
        return None;
    }
    let end = if length_digits == 0 {
        usize::MAX
    } else {
        start.saturating_add(decimal(&suffix[..length_digits]))
    };
    Some((3 + digits + 1 + length_digits + 1, start, end))
}

fn metadata(text: &str) -> Result<(&str, Option<String>)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim() != "---" {
        return Ok((text, None));
    }
    let mut offset = first.len();
    for line in lines {
        if line.trim() == "---" {
            let yaml = &text[first.len()..offset];
            let value: serde_yaml::Value = serde_yaml::from_str(yaml)
                .map_err(|_| anyhow::anyhow!("Invalid command metadata"))?;
            let description = match value {
                serde_yaml::Value::Null
                    if yaml.lines().all(|line| {
                        line.trim().is_empty() || line.trim_start().starts_with('#')
                    }) =>
                {
                    None
                }
                serde_yaml::Value::Mapping(mapping) => match mapping.get("description") {
                    None | Some(serde_yaml::Value::Null) => None,
                    Some(serde_yaml::Value::String(value)) => Some(value.clone()),
                    _ => anyhow::bail!("Invalid command metadata"),
                },
                _ => anyhow::bail!("Invalid command metadata"),
            };
            return Ok((&text[offset + line.len()..], description));
        }
        offset += line.len();
    }
    anyhow::bail!("YAML frontmatter is not closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(files: &[(&str, &[u8])]) -> (tempfile::TempDir, FileCommands) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("commands")).unwrap();
        for (name, text) in files {
            fs::write(dir.path().join("commands").join(name), text).unwrap();
        }
        let commands = FileCommands::from_dir(dir.path());
        (dir, commands)
    }

    fn expand(content: &str, args: &str) -> String {
        FileCommand {
            name: "/test".into(),
            description: String::new(),
            content: content.into(),
        }
        .expand(args)
    }

    #[test]
    fn file_command_quotes_and_placeholders() {
        for (args, expected) in [
            ("auth \"error handling\" tests", "auth|error handling|tests"),
            ("a' b'\" c\" d e", "a b c|d|e"),
            ("\"\" '' a\tb c", "a|b|c"),
            ("'a \"b\"' c d", "a \"b\"|c|d"),
            ("a \"unterminated value", "a|unterminated value|"),
            (r"a\ b c", r"a\|b|c"),
            ("a\u{a0}b c d", "a\u{a0}b|c|d"),
        ] {
            assert_eq!(expand("$1|$2|$3", args), expected, "{args:?}");
        }
        let args = "a b c d e f g h i j";
        for (body, expected) in [
            ("$0/$1/$10/$11/$99999999999999999999999999999", "/a/j//"),
            (
                "$@[2]/$@[2:]/$@[2:2]",
                "b c d e f g h i j/b c d e f g h i j/b c",
            ),
            (
                "$@[0]/$@[0:1]/$@[1:0]/$@[11]/$@[999999999999999999999999999999]",
                "////",
            ),
            ("$@[9:999999999999999999999999999999]", "i j"),
            (
                "$@[x]/$@[2:x]/$@[2:1/$ARGUMENTSuffix",
                "a b c d e f g h i j[x]/a b c d e f g h i j[2:x]/a b c d e f g h i j[2:1/a b c d e f g h i juffix",
            ),
            ("$ARGUMENTS/$@", "a b c d e f g h i j/a b c d e f g h i j"),
        ] {
            assert_eq!(expand(body, args), expected, "{body}");
        }
        assert_eq!(
            expand("$1|$2|$3", "'$1' '$@' '$ARGUMENTS'"),
            "$1|$@|$ARGUMENTS"
        );
        assert_eq!(expand("$@", ""), "");
        assert_eq!(
            expand("{{task}} $unknown \n", "a b"),
            "{{task}} $unknown\n\na b"
        );
        assert_eq!(expand("\\$1", "a"), "\\a");
    }

    #[test]
    fn file_command_parse_metadata_and_snapshot() {
        use super::super::{Command, parse};
        let (_, cmds) = load(&[
            ("quit-prompt.md", b"/quit"),
            ("shell-prompt.md", b"!printf not-shell"),
            ("empty.md", b"$2"),
            ("good.md", b"Good $1"),
        ]);
        assert!(parse("/empty a", &cmds).is_err());
        assert_eq!(
            parse("/good auth", &cmds).unwrap(),
            Some(Command::Prompt("Good auth".into()))
        );
        assert_eq!(
            parse("/quit-prompt", &cmds).unwrap(),
            Some(Command::Prompt("/quit".into()))
        );
        assert_eq!(
            parse("/shell-prompt", &cmds).unwrap(),
            Some(Command::Prompt("!printf not-shell".into()))
        );
        assert_eq!(parse("/unknown", &cmds).unwrap(), Some(Command::Unknown));
        assert_eq!(parse("/good\nordinary task", &cmds).unwrap(), None);

        let (_, meta) = load(&[
            ("review.md", "\u{feff}---\r\ndescription: ' Review   source '\r\nname: ignored\r\nallowed-tools: [bash]\r\n---\r\nReview $1.\r\n".as_bytes()),
            ("中文.md", format!("---\n---\n{}\n", "中".repeat(61)).as_bytes()),
            ("empty-description.md", b"---\ndescription: null\n---\n\n fallback \n"),
        ]);
        assert!(meta.diagnostics.is_empty());
        assert_eq!(
            (
                meta.entries[1].name.as_str(),
                meta.entries[1].description.as_str()
            ),
            ("/review", "Review source")
        );
        assert_eq!(
            meta.entries[2].description,
            format!("{}...", "中".repeat(60))
        );
        assert_eq!(meta.entries[0].description, "fallback");

        let (dir, snap) = load(&[("review.md", b"Old $1")]);
        fs::write(dir.path().join("commands/review.md"), "New $1").unwrap();
        fs::write(dir.path().join("commands/added.md"), "Added").unwrap();
        assert_eq!(
            parse("/review auth", &snap).unwrap(),
            Some(Command::Prompt("Old auth".into()))
        );
        assert_eq!(parse("/added", &snap).unwrap(), Some(Command::Unknown));
        let next = FileCommands::from_dir(dir.path());
        assert_eq!(
            parse("/review auth", &next).unwrap(),
            Some(Command::Prompt("New auth".into()))
        );
    }

    #[test]
    fn file_command_bad_files_missing_root_and_conflicts() {
        let (dir, _) = load(&[
            ("good.md", b"Good"),
            (".hidden.md", b"hidden"),
            ("upper.MD", b"ignored"),
            ("bad.md", b"---\ndescription: [\n---\nsecret"),
            ("open.md", b"---\nsecret"),
            ("blank.md", b" \n"),
            ("utf8.md", &[255]),
            ("help.md", b"override"),
            ("think.md", b"override"),
            ("models.md", b"override"),
            ("settings.md", b"override"),
            ("setting.md", b"override"),
            ("config.md", b"override"),
            ("bad name.md", b"invalid"),
            ("type.md", b"---\ndescription: 1\n---\nbody"),
        ]);
        fs::create_dir(dir.path().join("commands/nested.md")).unwrap();
        fs::write(dir.path().join("commands/nested.md/inner.md"), "ignored").unwrap();
        let commands = FileCommands::from_dir(dir.path());
        assert_eq!(
            commands
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["/good"]
        );
        assert_eq!(commands.diagnostics.len(), 12);
        assert!(
            commands
                .diagnostics
                .iter()
                .all(|i| i.contains("commands/") && !i.contains("secret"))
        );

        let empty = tempfile::tempdir().unwrap();
        let ce = FileCommands::from_dir(empty.path());
        assert!(
            ce.entries.is_empty()
                && ce.diagnostics.is_empty()
                && !empty.path().join("commands").exists()
        );
        fs::write(empty.path().join("commands"), "not a directory").unwrap();
        assert_eq!(FileCommands::from_dir(empty.path()).diagnostics.len(), 1);

        let (_dt, theme_cmd) = load(&[("theme.md", b"Override theme")]);
        assert!(theme_cmd.entries.is_empty() && theme_cmd.diagnostics[0].contains("/theme"));
    }

    #[cfg(unix)]
    #[test]
    fn file_command_unix_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (dir, _) = load(&[("unreadable.md", b"private"), ("good.md", b"Good")]);
        let path = dir.path().join("commands/unreadable.md");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
        let commands = FileCommands::from_dir(dir.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(commands.entries.len() == 1 && commands.diagnostics[0].contains("unreadable.md"));
        let root = dir.path().join("commands");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o0)).unwrap();
        let inaccessible = FileCommands::from_dir(dir.path());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(inaccessible.entries.is_empty() && inaccessible.diagnostics.len() == 1);

        let dir_sym = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::write(target.path().join("real.md"), "linked body").unwrap();
        symlink(target.path(), dir_sym.path().join("commands")).unwrap();
        symlink(
            target.path().join("real.md"),
            target.path().join("alias.md"),
        )
        .unwrap();
        symlink(
            target.path().join("missing"),
            target.path().join("broken.md"),
        )
        .unwrap();
        let sym_cmds = FileCommands::from_dir(dir_sym.path());
        assert_eq!((sym_cmds.entries.len(), sym_cmds.diagnostics.len()), (2, 1));
    }
}
