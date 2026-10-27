use crate::config;
use anyhow::{Context, Result, ensure};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) type SharedSkills = Arc<Mutex<Skills>>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SkillSource {
    Claude,
    Codex,
}

impl SkillSource {
    pub(crate) const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub(crate) fn roots(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["~/.claude/skills"],
            Self::Codex => &["~/.agents/skills", "~/.codex/skills"],
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Selection {
    pub(crate) sources: Vec<SkillSource>,
    pub(crate) skills: Vec<(String, bool)>,
}

#[derive(Clone, Debug)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: PathBuf,
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct SkillHeader {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) uri: String,
}

const URI_NAME: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

impl Skill {
    pub(crate) fn uri(&self) -> String {
        format!("skill://{}", utf8_percent_encode(&self.name, URI_NAME))
    }
}

#[derive(Debug)]
pub(crate) struct SkillTarget {
    pub(crate) path: PathBuf,
    pub(crate) base_dir: PathBuf,
}

fn decode_uri_part(value: &str) -> Result<std::borrow::Cow<'_, str>> {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%' {
            ensure!(
                bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                    && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit),
                "Invalid skill URI"
            );
        }
    }
    let decoded = percent_decode_str(value)
        .decode_utf8()
        .map_err(|_| anyhow::anyhow!("Invalid skill URI"))?;
    ensure!(!decoded.contains('\0'), "Invalid skill URI");
    Ok(decoded)
}

#[derive(Clone, Default)]
pub(crate) struct Skills {
    pub(crate) entries: Vec<Skill>,
    pub(crate) diagnostics: Vec<String>,
    pub(crate) sources: Vec<SkillSource>,
    settings_path: PathBuf,
    home: PathBuf,
}

#[derive(Deserialize)]
struct Metadata {
    name: String,
    description: String,
}

fn metadata(text: &str) -> Result<Metadata> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    ensure!(
        lines.next().is_some_and(|line| line.trim() == "---"),
        "Missing YAML frontmatter"
    );
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    ensure!(closed, "YAML frontmatter is not closed");
    let meta: Metadata =
        serde_yaml::from_str(&yaml).map_err(|_| anyhow::anyhow!("Invalid skill metadata"))?;
    ensure!(
        !meta.name.trim().is_empty() && !meta.description.trim().is_empty(),
        "Skill name and description must not be empty"
    );
    ensure!(
        !meta.name.chars().any(char::is_control),
        "Skill name must not contain control characters"
    );
    Ok(meta)
}

impl Skills {
    pub(crate) fn shared(self) -> SharedSkills {
        Arc::new(Mutex::new(self))
    }

    pub(crate) fn from_dir(dir: &Path) -> Self {
        let mut skills = Self {
            settings_path: dir.join("settings.yml"),
            home: dir
                .parent()
                .and_then(Path::parent)
                .unwrap_or(dir)
                .to_path_buf(),
            ..Self::default()
        };
        if let Err(error) = skills.reload() {
            skills.diagnostics.push(format!("{error:#}"));
        }
        skills
    }

    fn resolve(&self, input: &str) -> PathBuf {
        if input == "~" {
            self.home.clone()
        } else if let Some(suffix) = input.strip_prefix("~/") {
            self.home.join(suffix)
        } else {
            let path = PathBuf::from(input);
            if path.is_absolute() {
                path
            } else {
                self.settings_path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(path)
            }
        }
    }

    pub(crate) fn reload(&mut self) -> Result<()> {
        let settings = config::read_settings(&self.settings_path)?;
        *self = self.discover(settings);
        Ok(())
    }

    fn discover(&self, settings: config::Settings) -> Self {
        let disabled: HashSet<_> = settings.disabled_skills.into_iter().collect();
        let mut next = Self {
            settings_path: self.settings_path.clone(),
            home: self.home.clone(),
            sources: SkillSource::ALL
                .into_iter()
                .filter(|source| settings.skill_sources.contains(source))
                .collect(),
            ..Self::default()
        };
        let mut visited = HashSet::new();
        let mut files = HashSet::new();
        let roots = settings
            .skill_dirs
            .into_iter()
            .chain(
                next.sources
                    .iter()
                    .flat_map(|source| source.roots().iter().map(|root| (*root).to_owned())),
            )
            .collect::<Vec<_>>();
        for root in roots {
            let mut candidates = Vec::new();
            next.scan(&next.resolve(&root), &mut visited, &mut candidates);
            candidates.sort();
            for candidate in candidates {
                let result = (|| -> Result<_> {
                    let path =
                        fs::canonicalize(&candidate).context("Failed to resolve skill path")?;
                    if !files.insert(path.clone()) {
                        return Ok(None);
                    }
                    ensure!(
                        fs::metadata(&path)?.is_file(),
                        "SKILL.md must be a regular file"
                    );
                    let text = fs::read_to_string(&path).context("Failed to read SKILL.md")?;
                    let meta = metadata(&text)?;
                    if next.entries.iter().any(|skill| skill.name == meta.name) {
                        anyhow::bail!(
                            "Skill {} is already defined; keeping the first definition",
                            meta.name
                        );
                    }
                    Ok(Some(Skill {
                        enabled: !disabled.contains(&meta.name),
                        name: meta.name,
                        description: meta.description,
                        path,
                    }))
                })();
                match result {
                    Ok(Some(skill)) => next.entries.push(skill),
                    Ok(None) => {}
                    Err(error) => next
                        .diagnostics
                        .push(format!("{}: {error:#}", candidate.display())),
                }
            }
        }
        next
    }

    fn scan(&mut self, path: &Path, visited: &mut HashSet<PathBuf>, files: &mut Vec<PathBuf>) {
        let result = (|| -> Result<()> {
            let canonical = match fs::canonicalize(path) {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error).context("Unable to access skill directory"),
            };
            if !visited.insert(canonical) {
                return Ok(());
            }
            ensure!(path.is_dir(), "Skill root path must be a directory");
            let skill = path.join("SKILL.md");
            match fs::metadata(&skill) {
                Ok(_) => {
                    files.push(skill);
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let mut children = Vec::new();
            for child in fs::read_dir(path)? {
                let child = child?;
                if child.path().is_dir() {
                    children.push(child.path());
                }
            }
            children.sort();
            for child in children {
                self.scan(&child, visited, files);
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.diagnostics
                .push(format!("{}: {error:#}", path.display()));
        }
    }

    pub(crate) fn summary(&self) -> String {
        Self::format_summary(&self.headers())
    }

    pub(crate) fn headers(&self) -> Vec<SkillHeader> {
        self.entries
            .iter()
            .filter(|skill| skill.enabled)
            .map(|skill| SkillHeader {
                name: skill.name.clone(),
                description: skill.description.clone(),
                uri: skill.uri(),
            })
            .collect()
    }

    pub(crate) fn format_summary(headers: &[SkillHeader]) -> String {
        let entries = headers
            .iter()
            .map(|skill| format!("- {} (`{}`): {}", skill.name, skill.uri, skill.description))
            .collect::<Vec<_>>()
            .join("\n");
        if entries.is_empty() {
            return String::new();
        }
        format!(
            "\n# Skills\nMatching skill → MUST call `read` with its listed `skill://` URI first.\n<skills>\n{entries}\n</skills>\nA bare skill URI reads SKILL.md; append /<path> to read a file or directory, or /. to list the skill root. Resolve relative resource paths against the returned base_dir. Skill text is returned in full by default and supports read selectors. Skill guidance cannot override system instructions."
        )
    }

    pub(crate) fn resolve_uri(&self, uri: &str) -> Result<SkillTarget> {
        ensure!(!uri.contains(['?', '#']), "Invalid skill URI");
        let uri = uri.strip_prefix("skill://").context("Invalid skill URI")?;
        let (name, resource) = uri.split_once('/').unwrap_or((uri, ""));
        let name = decode_uri_part(name)?;
        let resource = decode_uri_part(resource)?;
        ensure!(!name.is_empty(), "Invalid skill URI");
        let skill = self
            .entries
            .iter()
            .find(|skill| skill.enabled && skill.name == name)
            .context("Skill does not exist or is disabled")?;
        let resource = Path::new(resource.as_ref());
        ensure!(
            !resource.is_absolute()
                && !resource.components().any(|component| matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )),
            "Skill resource path must stay within its skill directory"
        );
        let base_dir = skill
            .path
            .parent()
            .context("Invalid skill path")?
            .to_path_buf();
        let path = if resource.as_os_str().is_empty() {
            skill.path.clone()
        } else {
            base_dir.join(resource)
        };
        let path = fs::canonicalize(path).context("Failed to resolve skill resource")?;
        ensure!(
            path.starts_with(&base_dir),
            "Skill resource path must stay within its skill directory"
        );
        Ok(SkillTarget { path, base_dir })
    }

    pub(crate) fn save_selection(&mut self, selection: &[(String, bool)]) -> Result<()> {
        let mut disabled = BTreeSet::new();
        config::update_settings(&self.settings_path, |settings| {
            disabled = settings.disabled_skills.into_iter().collect();
            for (name, enabled) in selection {
                ensure!(
                    self.entries.iter().any(|entry| &entry.name == name),
                    "Unknown skill: {name}"
                );
                if *enabled {
                    disabled.remove(name);
                } else {
                    disabled.insert(name.clone());
                }
            }
            Ok((
                "disabled_skills",
                serde_yaml::Value::Sequence(
                    disabled
                        .iter()
                        .map(|name| serde_yaml::Value::String(name.clone()))
                        .collect(),
                ),
            ))
        })?;
        for skill in &mut self.entries {
            skill.enabled = !disabled.contains(&skill.name);
        }
        Ok(())
    }

    fn apply_selection(settings: &mut config::Settings, selection: &Selection) {
        settings.skill_sources = selection.sources.clone();
        let mut disabled: BTreeSet<_> = settings.disabled_skills.drain(..).collect();
        for (name, enabled) in &selection.skills {
            if *enabled {
                disabled.remove(name);
            } else {
                disabled.insert(name.clone());
            }
        }
        settings.disabled_skills = disabled.into_iter().collect();
    }

    pub(crate) fn preview(&self, selection: &Selection) -> Result<Self> {
        let mut settings = config::read_settings(&self.settings_path)?;
        Self::apply_selection(&mut settings, selection);
        Ok(self.discover(settings))
    }

    pub(crate) fn save_management(&mut self, selection: &Selection) -> Result<()> {
        let mut next = None;
        config::update_settings_fields(&self.settings_path, |mut settings| {
            Self::apply_selection(&mut settings, selection);
            let fields = vec![
                (
                    "skill_sources",
                    serde_yaml::to_value(&settings.skill_sources)?,
                ),
                (
                    "disabled_skills",
                    serde_yaml::to_value(&settings.disabled_skills)?,
                ),
            ];
            next = Some(self.discover(settings));
            Ok(fields)
        })?;
        *self = next.context("Skill selection was not prepared")?;
        Ok(())
    }

    pub(crate) fn save_source(&mut self, source: SkillSource, enabled: bool) -> Result<()> {
        let mut next = None;
        config::update_settings(&self.settings_path, |mut settings| {
            settings.skill_sources.retain(|value| *value != source);
            if enabled {
                settings.skill_sources.push(source);
            }
            let value = serde_yaml::to_value(&settings.skill_sources)?;
            next = Some(self.discover(settings));
            Ok(("skill_sources", value))
        })?;
        *self = next.context("Skill source selection was not prepared")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".pp/agent");
        fs::create_dir_all(&dir).unwrap();
        (home, dir)
    }
    fn skill(root: &Path, folder: &str, name: &str, body: &str) -> PathBuf {
        let path = root.join(folder).join("SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("---\nname: {name}\ndescription: >\n  用于测试\n  YAML 多行说明\nextra: [one, two]\n---\n{body}\n")).unwrap();
        path
    }
    #[test]
    fn skill_uri_encoding_enablement_and_resource_isolation() {
        let (_home, dir) = fixture();
        for (index, name) in ["rust", "团队 rust", "team/rust", "a:b", "100%", "a", "a/b"]
            .into_iter()
            .enumerate()
        {
            skill(&dir.join("skills"), &index.to_string(), name, "body");
        }
        let mut skills = Skills::from_dir(&dir);
        for entry in &skills.entries {
            let target = skills.resolve_uri(&entry.uri()).unwrap();
            assert_eq!(target.path, entry.path);
            assert_eq!(target.base_dir, entry.path.parent().unwrap());
            assert_eq!(
                skills
                    .resolve_uri(&format!("{}/", entry.uri()))
                    .unwrap()
                    .path,
                entry.path
            );
        }
        let root = skills.resolve_uri("skill://a/.").unwrap().path;
        fs::write(root.join("b"), "resource").unwrap();
        assert_eq!(
            skills.resolve_uri("skill://a/b").unwrap().path,
            root.join("b")
        );
        assert_ne!(
            skills.resolve_uri("skill://a%2Fb").unwrap().path,
            root.join("b")
        );
        for uri in [
            "skill://",
            "skill://rust%",
            "skill://rust/%GG",
            "skill://%FF",
            "skill://rust/%00",
            "skill://rust?x",
            "skill://rust/#x",
        ] {
            assert_eq!(
                skills.resolve_uri(uri).unwrap_err().to_string(),
                "Invalid skill URI",
                "{uri}"
            );
        }
        for uri in ["skill://a/%2e%2e/out", "skill://a/%2Ftmp"] {
            assert_eq!(
                skills.resolve_uri(uri).unwrap_err().to_string(),
                "Skill resource path must stay within its skill directory"
            );
        }
        let outside = dir.join("canary");
        fs::write(&outside, "outside").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("b"), root.join("inside")).unwrap();
        assert!(skills.resolve_uri("skill://a/outside").is_err());
        assert_eq!(
            skills.resolve_uri("skill://a/inside").unwrap().path,
            root.join("b")
        );
        assert!(skills.resolve_uri("skill://a/missing").is_err());
        skills
            .entries
            .iter_mut()
            .find(|entry| entry.name == "a")
            .unwrap()
            .enabled = false;
        assert_eq!(
            skills.resolve_uri("skill://a").unwrap_err().to_string(),
            "Skill does not exist or is disabled"
        );
    }

    #[test]
    fn inherited_sources_preview_precedence_and_selection_are_transactional() {
        let (home, dir) = fixture();
        skill(&dir.join("skills"), "native", "same", "pp version");
        skill(
            &home.path().join(".claude/skills"),
            "same",
            "same",
            "claude version",
        );
        skill(
            &home.path().join(".claude/skills"),
            "a",
            "claude-only",
            "claude body",
        );
        skill(
            &home.path().join(".agents/skills"),
            "a",
            "codex-only",
            "codex body",
        );
        skill(
            &home.path().join(".codex/skills"),
            "a",
            "codex-legacy",
            "legacy body",
        );
        let path = dir.join("settings.yml");
        fs::write(
            &path,
            "# 保留注释\nthink_level: high\ndisabled_skills: [not-installed]\n",
        )
        .unwrap();
        let mut skills = Skills::from_dir(&dir);
        assert_eq!(skills.entries.len(), 1);
        let selection = Selection {
            sources: vec![SkillSource::Codex, SkillSource::Claude, SkillSource::Codex],
            skills: vec![("claude-only".into(), false)],
        };
        let preview = skills.preview(&selection).unwrap();
        assert_eq!(preview.sources, SkillSource::ALL);
        assert_eq!(preview.entries.len(), 4);
        assert_eq!(preview.diagnostics.len(), 1);
        assert_eq!(
            preview.resolve_uri("skill://same").unwrap().path,
            dir.join("skills/native/SKILL.md").canonicalize().unwrap()
        );
        assert!(preview.resolve_uri("skill://claude-only").is_err());
        assert!(preview.resolve_uri("skill://codex-only").is_ok());
        assert!(preview.resolve_uri("skill://codex-legacy").is_ok());
        assert_eq!(skills.entries.len(), 1);
        assert!(!fs::read_to_string(&path).unwrap().contains("skill_sources"));
        skills.save_management(&selection).unwrap();
        let persisted = fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("# 保留注释"));
        assert!(persisted.contains("think_level: high"));
        assert!(persisted.contains("not-installed"));
        let mut restored = Skills::from_dir(&dir);
        assert!(restored.resolve_uri("skill://claude-only").is_err());
        restored.save_source(SkillSource::Claude, false).unwrap();
        assert!(
            !restored
                .entries
                .iter()
                .any(|skill| skill.name == "claude-only")
        );
        restored.save_source(SkillSource::Claude, true).unwrap();
        assert!(restored.resolve_uri("skill://claude-only").is_err());
        fs::write(&path, "invalid yaml").unwrap();
        assert!(skills.save_management(&Selection::default()).is_err());
        assert_eq!(skills.entries.len(), 4);
        assert_eq!(skills.sources, SkillSource::ALL);
    }

    #[cfg(unix)]
    #[test]
    fn inherited_symlinked_sources_share_one_definition() {
        use std::os::unix::fs::symlink;
        let (home, dir) = fixture();
        skill(&home.path().join(".claude/skills"), "a", "shared", "body");
        fs::create_dir_all(home.path().join(".agents")).unwrap();
        symlink(
            home.path().join(".claude/skills"),
            home.path().join(".agents/skills"),
        )
        .unwrap();
        fs::write(dir.join("settings.yml"), "skill_sources: [claude, codex]\n").unwrap();
        let skills = Skills::from_dir(&dir);
        assert_eq!(skills.entries.len(), 1);
        assert!(skills.diagnostics.is_empty());
    }

    #[test]
    fn discovery_defaults_metadata_duplicates_nested_roots_and_errors() {
        let (_home, dir) = fixture();
        let first = skill(
            &dir.join("skills"),
            "a",
            "rust",
            "private full instructions",
        );
        skill(&dir.join("skills"), "b", "rust", "duplicate");
        skill(
            first.parent().unwrap(),
            "resources/nested",
            "ignored",
            "ignored",
        );
        fs::create_dir_all(dir.join("skills/bad")).unwrap();
        fs::write(dir.join("skills/bad/SKILL.md"), "---\nname: bad\n---").unwrap();
        let skills = Skills::from_dir(&dir);
        assert_eq!(skills.entries.len(), 1);
        assert_eq!(skills.entries[0].name, "rust");
        assert!(skills.entries[0].enabled);
        assert!(skills.entries[0].description.contains("YAML 多行说明"));
        assert_eq!(skills.diagnostics.len(), 2);
        assert!(!skills.summary().contains("private full instructions"));
        assert_eq!(
            skills.resolve_uri("skill://rust").unwrap().path,
            first.canonicalize().unwrap()
        );
        assert!(skills.resolve_uri("skill://../rust").is_err());
    }
    #[test]
    fn missing_default_root_is_silent_and_reload_discovers_new_skills() {
        let (_home, dir) = fixture();
        let mut skills = Skills::from_dir(&dir);
        assert!(skills.entries.is_empty());
        assert!(skills.diagnostics.is_empty());
        assert!(!dir.join("skills").exists());

        skill(&dir.join("skills"), "rust", "rust", "body");
        skills.reload().unwrap();
        assert_eq!(skills.entries.len(), 1);
        assert!(skills.diagnostics.is_empty());

        fs::remove_dir_all(dir.join("skills")).unwrap();
        skills.reload().unwrap();
        assert!(skills.entries.is_empty());
        assert!(skills.diagnostics.is_empty());
    }
    #[test]
    fn non_directory_root_still_reports_a_diagnostic() {
        let (_home, dir) = fixture();
        fs::write(dir.join("skills"), "not a directory").unwrap();
        let skills = Skills::from_dir(&dir);
        assert!(skills.entries.is_empty());
        assert_eq!(skills.diagnostics.len(), 1);
    }
    #[test]
    fn configurable_roots_order_relative_absolute_tilde_and_disabled() {
        let (home, dir) = fixture();
        skill(&dir.join("relative"), "z", "same", "first");
        skill(&home.path().join("other"), "a", "same", "second");
        skill(&home.path().join("other"), "b", "other", "third");
        fs::write(dir.join("settings.yml"), json!({"skill_dirs":["relative", "~/other", home.path().join("other"), "missing"],"disabled_skills":["same"]}).to_string()).unwrap();
        let skills = Skills::from_dir(&dir);
        assert_eq!(skills.entries.len(), 2);
        assert!(!skills.entries[0].enabled);
        assert!(
            skills.entries[0]
                .path
                .starts_with(fs::canonicalize(dir.join("relative")).unwrap())
        );
        assert!(skills.resolve_uri("skill://same").is_err());
        assert!(!skills.summary().contains("same"));
        assert!(skills.summary().contains("other"));
        assert_eq!(skills.diagnostics.len(), 1);
        fs::write(dir.join("settings.yml"), r#"{"skill_dirs":[]}"#).unwrap();
        let mut skills = skills;
        skills.reload().unwrap();
        assert!(skills.entries.is_empty());
        assert!(skills.diagnostics.is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_deduplicated_and_cycles_terminate() {
        use std::os::unix::fs::symlink;
        let (_home, dir) = fixture();
        skill(&dir.join("skills"), "real", "linked", "body");
        symlink(dir.join("skills"), dir.join("skills/cycle")).unwrap();
        symlink(dir.join("skills/real"), dir.join("skills/alias")).unwrap();
        let skills = Skills::from_dir(&dir);
        assert_eq!(skills.entries.len(), 1);
        assert!(skills.diagnostics.is_empty());
    }
    #[test]
    fn saves_preserve_comments_other_fields_permissions_and_unknown_disabled_names() {
        let (_home, dir) = fixture();
        skill(&dir.join("skills"), "a", "rust", "body");
        let path = dir.join("settings.yml");
        fs::write(
            &path,
            "# 保留此注释\nthink_level: high\ndisabled_skills: [not-installed] # 保留行尾注释\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        }
        let mut skills = Skills::from_dir(&dir);
        skills.save_selection(&[("rust".into(), false)]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 保留此注释"));
        assert!(text.contains("# 保留行尾注释"));
        assert!(text.contains("think_level: high"));
        let settings = config::read_settings(&path).unwrap();
        assert_eq!(settings.disabled_skills, ["not-installed", "rust"]);
        assert!(!skills.entries[0].enabled);
        assert!(!Skills::from_dir(&dir).entries[0].enabled);
        skills.save_selection(&[("rust".into(), true)]).unwrap();
        assert!(Skills::from_dir(&dir).entries[0].enabled);
    }
    #[test]
    fn missing_settings_is_created_and_failed_save_or_reload_does_not_mutate_catalog() {
        let (_home, dir) = fixture();
        skill(&dir.join("skills"), "a", "rust", "body");
        let mut skills = Skills::from_dir(&dir);
        skills.save_selection(&[("rust".into(), false)]).unwrap();
        assert!(dir.join("settings.yml").is_file());
        fs::write(dir.join("settings.yml"), "invalid yaml").unwrap();
        assert!(skills.save_selection(&[("rust".into(), true)]).is_err());
        assert!(skills.reload().is_err());
        assert!(!skills.entries[0].enabled);
        assert_eq!(
            fs::read_to_string(dir.join("settings.yml")).unwrap(),
            "invalid yaml"
        );
        assert!(skills.resolve_uri("skill://rust").is_err());
    }
    #[test]
    fn invalid_metadata_utf8_and_deleted_skill_are_local_failures() {
        let (_home, dir) = fixture();
        let path = skill(&dir.join("skills"), "a", "rust", "body");
        let skills = Skills::from_dir(&dir);
        fs::remove_file(&path).unwrap();
        assert!(skills.resolve_uri("skill://rust").is_err());
        fs::write(&path, [0xff]).unwrap();
        assert!(Skills::from_dir(&dir).entries.is_empty());
        for text in [
            "body",
            "---\nname: rust",
            "---\nname: ''\ndescription: test\n---",
            "---\nname: rust\ndescription: []\n---",
        ] {
            assert!(metadata(text).is_err(), "{text}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn failed_atomic_save_keeps_file_and_runtime_state_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let (_home, dir) = fixture();
        skill(&dir.join("skills"), "a", "rust", "body");
        let path = dir.join("settings.yml");
        fs::write(&path, "{} # 原始配置\n").unwrap();
        let mut skills = Skills::from_dir(&dir);
        let permissions = fs::metadata(&dir).unwrap().permissions();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let result = skills.save_selection(&[("rust".into(), false)]);
        let management_result = skills.save_management(&Selection {
            sources: vec![SkillSource::Claude],
            skills: vec![("rust".into(), false)],
        });
        fs::set_permissions(&dir, permissions).unwrap();
        assert!(result.is_err());
        assert!(management_result.is_err());
        assert!(skills.sources.is_empty());
        assert!(skills.entries[0].enabled);
        assert_eq!(fs::read_to_string(path).unwrap(), "{} # 原始配置\n");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    }
}
