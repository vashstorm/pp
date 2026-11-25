//! Codex `permission.toml` TOML 编解码与私有文件元数据/规范路径工具。

use super::{
    BashRule, DEFAULT_PROFILE, Document, MatchMode, NetworkDomainDecision, Profile, SandboxGrant,
    validate_profile_name,
};
use anyhow::{Context, Result, anyhow, ensure};
use pp_config::config::PermissionPolicy;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

impl Document {
    pub(super) fn from_toml_str(text: &str) -> Result<Self> {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(text)
            .map_err(|_| anyhow!("invalid TOML permission rules config: check syntax"))?;
        let mut doc = Document::default();
        for (key, value) in table {
            match key.as_str() {
                "default_profile" => {
                    doc.default_profile = value
                        .try_into::<String>()
                        .map_err(|_| anyhow!("default_profile must be a string"))?;
                }
                "rules" => {
                    doc.rules = value
                        .try_into::<Vec<BashRule>>()
                        .map_err(|e| anyhow!("invalid [[rules]] entry: {e}"))?;
                }
                "sandbox" => {
                    doc.sandbox = value
                        .try_into::<Vec<SandboxGrant>>()
                        .map_err(|e| anyhow!("invalid [[sandbox]] entry: {e}"))?;
                }
                "tools" | "bash" | "version" | "permissions" => {}
                profile_name if profile_name == DEFAULT_PROFILE || value.is_table() => {
                    validate_profile_name(profile_name)?;
                    let profile: Profile = value
                        .try_into()
                        .map_err(|e| anyhow!("invalid [{profile_name}] profile: {e}"))?;
                    doc.profiles.insert(profile_name.to_owned(), profile);
                }
                _ => {}
            }
        }
        doc.validate()?;
        Ok(doc)
    }
}

pub(super) fn policy_str(policy: PermissionPolicy) -> &'static str {
    match policy {
        PermissionPolicy::Allow => "allow",
        PermissionPolicy::Prompt => "prompt",
        PermissionPolicy::Deny => "deny",
    }
}

pub(super) fn encode_document(document: &Document) -> Result<String> {
    let mut out = String::new();
    if document.is_empty() {
        writeln!(out, "default_profile = \"default\"")?;
    } else {
        if document.default_profile != DEFAULT_PROFILE {
            write!(out, "default_profile = ")?;
            write_toml_string(&mut out, &document.default_profile)?;
            writeln!(out)?;
        }
        for (name, profile) in &document.profiles {
            if profile.is_empty() {
                continue;
            }
            if profile.description.is_some() || profile.extends.is_some() {
                if !out.is_empty() {
                    writeln!(out)?;
                }
                write_table_header(&mut out, &[name])?;
                if let Some(desc) = &profile.description {
                    write!(out, "description = ")?;
                    write_toml_string(&mut out, desc)?;
                    writeln!(out)?;
                }
                if let Some(extends) = &profile.extends {
                    write!(out, "extends = ")?;
                    write_toml_string(&mut out, extends)?;
                    writeln!(out)?;
                }
            }
            if !profile.filesystem.host.is_empty() {
                if !out.is_empty() {
                    writeln!(out)?;
                }
                write_table_header(&mut out, &[name, "filesystem"])?;
                for (path, access) in &profile.filesystem.host {
                    write_toml_string(&mut out, path)?;
                    writeln!(out, " = \"{}\"", access.as_str())?;
                }
            }
            if !profile.filesystem.workspace.is_empty() {
                if !out.is_empty() {
                    writeln!(out)?;
                }
                write_table_header(&mut out, &[name, "filesystem", ":workspace"])?;
                for (path, access) in &profile.filesystem.workspace {
                    write_toml_string(&mut out, path)?;
                    writeln!(out, " = \"{}\"", access.as_str())?;
                }
            }
            if !profile.network.is_empty() {
                if !out.is_empty() {
                    writeln!(out)?;
                }
                write_table_header(&mut out, &[name, "network"])?;
                if let Some(enabled) = profile.network.enabled {
                    writeln!(out, "enabled = {enabled}")?;
                }
                if !profile.network.domains.is_empty() {
                    writeln!(out)?;
                    write_table_header(&mut out, &[name, "network", "domains"])?;
                    for (domain, decision) in &profile.network.domains {
                        write_toml_string(&mut out, domain)?;
                        let val = match decision {
                            NetworkDomainDecision::Allow => "allow",
                            NetworkDomainDecision::Deny => "deny",
                        };
                        writeln!(out, " = \"{val}\"")?;
                    }
                }
            }
            if !profile.tools.is_empty() {
                if !out.is_empty() {
                    writeln!(out)?;
                }
                write_table_header(&mut out, &[name, "tools"])?;
                for (tool, policy) in &profile.tools {
                    write_toml_string(&mut out, tool)?;
                    writeln!(out, " = \"{}\"", policy_str(*policy))?;
                }
            }
        }
        for rule in &document.rules {
            if !out.is_empty() {
                writeln!(out)?;
            }
            writeln!(out, "[[rules]]")?;
            write!(out, "pattern = [")?;
            for (index, token) in rule.pattern.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_toml_string(&mut out, token)?;
            }
            writeln!(out, "]")?;
            if rule.match_mode != MatchMode::Exact {
                writeln!(out, "match = \"{}\"", rule.match_mode.as_str())?;
            }
            writeln!(out, "decision = \"{}\"", policy_str(rule.policy))?;
            if let Some(justification) = &rule.justification {
                write!(out, "justification = ")?;
                write_toml_string(&mut out, justification)?;
                writeln!(out)?;
            }
        }
        for grant in &document.sandbox {
            if !out.is_empty() {
                writeln!(out)?;
            }
            writeln!(out, "[[sandbox]]")?;
            write!(out, "command = ")?;
            write_toml_string(&mut out, &grant.command)?;
            writeln!(out)?;
            if !grant.writable.is_empty() {
                write_path_array(&mut out, "writable", &grant.writable)?;
            }
            if !grant.allow_protected.is_empty() {
                write_path_array(&mut out, "allow_protected", &grant.allow_protected)?;
            }
            if grant.network {
                writeln!(out, "network = true")?;
            }
        }
    }
    let decoded =
        Document::from_toml_str(&out).map_err(|_| anyhow!("cannot encode permission rules"))?;
    ensure!(
        normalized_for_roundtrip(&decoded) == normalized_for_roundtrip(document),
        "cannot encode permission rules"
    );
    Ok(out)
}

fn normalized_for_roundtrip(
    doc: &Document,
) -> (
    &str,
    BTreeMap<&String, &Profile>,
    &[BashRule],
    &[SandboxGrant],
) {
    let non_empty_profiles = doc
        .profiles
        .iter()
        .filter(|(_, p)| !p.is_empty())
        .collect::<BTreeMap<_, _>>();
    (
        &doc.default_profile,
        non_empty_profiles,
        &doc.rules,
        &doc.sandbox,
    )
}

fn write_path_array(out: &mut String, field: &str, paths: &[PathBuf]) -> Result<()> {
    write!(out, "{field} = [")?;
    for (index, path) in paths.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        let path = path
            .to_str()
            .context("permission rule path must be valid UTF-8")?;
        write_toml_string(out, path)?;
    }
    writeln!(out, "]")?;
    Ok(())
}

fn write_table_header(out: &mut String, segments: &[&str]) -> Result<()> {
    out.push('[');
    for (idx, segment) in segments.iter().enumerate() {
        if idx > 0 {
            out.push('.');
        }
        if is_bare_toml_key(segment) {
            out.push_str(segment);
        } else {
            write_toml_string(out, segment)?;
        }
    }
    writeln!(out, "]")?;
    Ok(())
}

fn is_bare_toml_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn write_toml_string(out: &mut String, value: &str) -> Result<()> {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            ch if ch.is_control() => write!(out, "\\u{:04X}", ch as u32)?,
            ch => out.push(ch),
        }
    }
    out.push('"');
    Ok(())
}

pub(super) fn check_private_meta(meta: &fs::Metadata) -> Result<()> {
    ensure!(
        meta.file_type().is_file()
            && meta.nlink() == 1
            && meta.mode() & 0o077 == 0
            && meta.uid() == rustix::process::geteuid().as_raw(),
        "permission file must be a private regular file owned by the current user"
    );
    Ok(())
}

pub(super) fn private_file(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    check_private_meta(&file.metadata()?)?;
    Ok(file)
}

pub(super) fn normalize_against_existing_ancestor(path: &Path) -> Option<PathBuf> {
    if let Ok(canon) = path.canonicalize() {
        return Some(canon);
    }
    let mut cur = path;
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while !cur.as_os_str().is_empty() {
        let Some(name) = cur.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        cur = cur.parent()?;
        if let Ok(mut base) = cur.canonicalize() {
            for part in suffix.into_iter().rev() {
                base.push(part);
            }
            return Some(lexically_clean(&base));
        }
    }
    None
}

fn lexically_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(comp.as_os_str()),
        }
    }
    out
}

pub(super) fn platform_path_eq(left: &Path, right: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut l = left.components();
        let mut r = right.components();
        loop {
            match (l.next(), r.next()) {
                (None, None) => return true,
                (Some(Component::Normal(a)), Some(Component::Normal(b))) => {
                    if a == b {
                        continue;
                    }
                    let (Some(sa), Some(sb)) = (a.to_str(), b.to_str()) else {
                        return false;
                    };
                    if sa.is_ascii() && sb.is_ascii() {
                        if !sa.eq_ignore_ascii_case(sb) {
                            return false;
                        }
                    } else if sa.to_lowercase() != sb.to_lowercase() {
                        return false;
                    }
                }
                (Some(a), Some(b)) if a == b => {}
                _ => return false,
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        left == right
    }
}
