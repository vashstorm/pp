//! LSP server definition parsing, merging and local discovery.
//!
//! Built-in definitions come from oh-my-pi (MIT License) at the pinned commit
//! `1c291b87e4b889398cc4b8e4ccdfa650b56a0486`,
//! file `packages/coding-agent/src/lsp/defaults.json`, with only TypeScript's
//! `hostInfo` changed to `pp`. Configuration reads only `~/.pp/agent/lsp.json` and
//! `<startup-cwd>/.pp/lsp.json`, never a plugin marketplace, Claude/Codex directories or
//! YAML variants. Every parse error reports only the source file, field path and error category, never echoing commands,
//! arguments or configuration values.

use anyhow::{Context, Result, bail, ensure};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const DEFAULT_DEFINITIONS: &str = include_str!("defaults.json");

const READY_FIELDS: &[&str] = &["timeoutMs", "pollMs", "settleMs", "statusRequestTimeoutMs"];

const PYTHON_ROOT_MARKERS: &[&str] = &[
    "pyproject.toml",
    "ty.toml",
    "requirements.txt",
    "setup.py",
    "setup.cfg",
    "Pipfile",
    "pyrightconfig.json",
    "ruff.toml",
    ".ruff.toml",
];

const NODE_ROOT_MARKERS: &[&str] = &[
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
];

const RUBY_ROOT_MARKERS: &[&str] = &["Gemfile", "Gemfile.lock"];
const GO_ROOT_MARKERS: &[&str] = &["go.mod", "go.sum", "go.work"];

/// rust-analyzer `workspace/ready` polling parameters; `None` keeps the module default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReadyTimings {
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) poll_ms: Option<u64>,
    pub(crate) settle_ms: Option<u64>,
    pub(crate) status_request_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct ServerDefinition {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) file_types: Vec<String>,
    pub(crate) language_id: Option<String>,
    pub(crate) root_markers: Vec<String>,
    pub(crate) init_options: Value,
    pub(crate) settings: Value,
    pub(crate) disabled: bool,
    pub(crate) warmup_timeout_ms: Option<u64>,
    pub(crate) is_linter: bool,
    pub(crate) _capabilities: Value,
    pub(crate) ready: ReadyTimings,
    /// Absolute path of the executable resolved during discovery; `None` when not discovered.
    pub(crate) resolved_command: Option<PathBuf>,
}

impl ServerDefinition {
    /// Actual arguments used to spawn: `$PID` is replaced with the current process ID (omnisharp).
    pub(crate) fn spawn_args(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|arg| {
                if arg == "$PID" {
                    std::process::id().to_string()
                } else {
                    arg.clone()
                }
            })
            .collect()
    }

    pub(crate) fn supports_file(&self, path: &Path) -> bool {
        let extension = path
            .extension()
            .map(|value| format!(".{}", value.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or_default();
        let extension_no_dot = extension.strip_prefix('.').unwrap_or("").to_owned();
        let file_name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        self.file_types.iter().any(|file_type| {
            let normalized = file_type.to_ascii_lowercase();
            let normalized_no_dot = normalized.trim_start_matches('.').to_owned();
            normalized == extension
                || normalized == file_name
                || normalized_no_dot == extension_no_dot
                || normalized_no_dot == file_name
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    /// Discovered servers (marker matched and executable present), in definition order.
    pub(crate) servers: Vec<ServerDefinition>,
    /// Full merged definition table (including undiscovered and disabled entries), used by status/capabilities.
    pub(crate) definitions: Vec<ServerDefinition>,
    pub(crate) idle_timeout_ms: Option<u64>,
}

impl Config {
    /// Read the user (supplied explicitly) and project `lsp.json`, merge built-in definitions and do local discovery.
    ///
    /// The user-level path must be injected by the caller from the config directory; HOME is never read here, so tests or a
    /// non-default config directory cannot accidentally read the developer's personal configuration.
    pub(crate) fn load_with(cwd: &Path, user: Option<&Path>) -> Result<Self> {
        let layers = layer_paths(cwd, user);
        let mut merged = merge_layers(&layers)?;
        let definitions: Vec<ServerDefinition> =
            merged.drain(..).map(|entry| entry.definition).collect();
        let discovered = discover(cwd, &definitions);
        let idle_timeout_ms = read_idle_timeout(&layers)?;
        Ok(Self {
            servers: discovered,
            definitions,
            idle_timeout_ms,
        })
    }

    pub(crate) fn server(&self, name: &str) -> Option<&ServerDefinition> {
        self.servers.iter().find(|server| server.name == name)
    }

    /// Servers matching the file, non-linters first, preserving definition order within a tier.
    pub(crate) fn servers_for_file(&self, path: &Path) -> Vec<&ServerDefinition> {
        let mut matches: Vec<&ServerDefinition> = self
            .servers
            .iter()
            .filter(|server| server.supports_file(path))
            .collect();
        matches.sort_by_key(|server| u8::from(server.is_linter));
        matches
    }

    /// LSP servers matching a concrete file (excluding CLI adapters such as Biome/SwiftLint).
    pub(crate) fn real_servers_for_file(&self, path: &Path) -> Vec<&ServerDefinition> {
        self.servers_for_file(path)
            .into_iter()
            .filter(|server| !crate::adapters::is_adapter(&server.name))
            .collect()
    }
}

fn read_idle_timeout(paths: &LayerPaths) -> Result<Option<u64>> {
    let mut idle = None;
    for path in paths.iter() {
        let Some(text) = read_source(path)? else {
            continue;
        };
        let raw = parse_raw(&text, path)?;
        if let Some(value) = raw.get("idleTimeoutMs") {
            idle = Some(expect_u64(value, "idleTimeoutMs", path)?);
        }
    }
    Ok(idle)
}

/// Configuration layer order: user → project; the built-in layer is added separately at the merge entry point.
struct LayerPaths {
    user: Option<PathBuf>,
    project: PathBuf,
}

impl LayerPaths {
    fn iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.user.iter().chain(std::iter::once(&self.project))
    }
}

fn layer_paths(cwd: &Path, user: Option<&Path>) -> LayerPaths {
    LayerPaths {
        user: user.map(Path::to_path_buf),
        project: cwd.join(".pp").join("lsp.json"),
    }
}

/// Read one configuration source; missing is normal, any other unreadable state is a configuration error.
fn read_source(path: &Path) -> Result<Option<String>> {
    let fd = match rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "LSP config {} cannot be opened without symlinks",
                    path.display()
                )
            });
        }
    };
    let mut file = std::fs::File::from(fd);
    ensure!(
        file.metadata()
            .with_context(|| format!("LSP config {} cannot be inspected", path.display()))?
            .is_file(),
        "LSP config {} must be a regular file",
        path.display()
    );
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text)
        .with_context(|| format!("LSP config {} cannot be read as UTF-8", path.display()))?;
    Ok(Some(text))
}

type RawFields = Vec<(String, Raw)>;
type LayerEntry = (String, RawFields, String);

/// Source label for the built-in layer; keeps the same error text as before.
const BUILTIN_LABEL: &str = "<builtin lsp defaults>";

/// Parse and field-validation result of the built-in default layer (`defaults.json`).
///
/// It is a compile-time constant, independent of cwd, `lsp.json` content and settings, so the whole process
/// parses it once; each Registry still reads the user/project layers itself and does local discovery.
static BUILTIN_ENTRIES: OnceLock<Vec<LayerEntry>> = OnceLock::new();

fn builtin_entries() -> Result<&'static [LayerEntry]> {
    if let Some(entries) = BUILTIN_ENTRIES.get() {
        return Ok(entries);
    }
    let source = Source::new(BUILTIN_LABEL.to_owned(), DEFAULT_DEFINITIONS);
    let raw = parse_raw(&source.text, source.path())?;
    let (servers, _) = split_layer(&raw, &source)?;
    let mut entries = Vec::with_capacity(servers.len());
    for (name, value) in servers {
        validate_server_shape(&name, &value, &source)?;
        entries.push((name, object_fields(&value), BUILTIN_LABEL.to_owned()));
    }
    // The built-in layer must itself be complete definitions; error attribution matches per-layer validation.
    for (name, fields, label) in &entries {
        finish_definition(name, fields, label)?;
    }
    Ok(BUILTIN_ENTRIES.get_or_init(|| entries))
}

struct MergedEntry {
    definition: ServerDefinition,
}

/// Merge the built-in → user → project definitions.
fn merge_layers(paths: &LayerPaths) -> Result<Vec<MergedEntry>> {
    // The built-in layer comes from the process-level cache: only the field tables are cloned here, the 53 definitions are not re-parsed.
    let mut entries: Vec<LayerEntry> = builtin_entries()?.to_vec();
    for path in paths.iter() {
        let Some(text) = read_source(path)? else {
            continue;
        };
        let source = Source::new(path.display().to_string(), &text);
        apply_layer(&mut entries, &source)?;
    }
    let mut merged = Vec::new();
    for (name, fields, source) in entries {
        let definition = finish_definition(&name, &fields, &source)?;
        merged.push(MergedEntry { definition });
    }
    Ok(merged)
}

struct Source {
    label: String,
    text: String,
}

impl Source {
    fn new(label: String, text: &str) -> Self {
        Self {
            label,
            text: text.to_owned(),
        }
    }

    fn path(&self) -> &Path {
        Path::new(&self.label)
    }
}

fn apply_layer(entries: &mut Vec<LayerEntry>, source: &Source) -> Result<()> {
    let raw = parse_raw(&source.text, source.path())?;
    let (servers, _) = split_layer(&raw, source)?;
    for (name, value) in servers {
        validate_server_shape(&name, &value, source)?;
        match entries
            .iter_mut()
            .find(|(existing, _, _)| *existing == name)
        {
            Some((_, fields, label)) => {
                merge_fields(fields, &value);
                *label = source.label.clone();
            }
            None => entries.push((name, object_fields(&value), source.label.clone())),
        }
    }
    // The built-in layer and every layer override must still be complete definitions after merging; error attribution points at the layer
    // that last modified the server.
    for (name, fields, label) in entries.iter() {
        finish_definition(name, fields, label)?;
    }
    Ok(())
}

fn split_layer(raw: &Raw, source: &Source) -> Result<(RawFields, Option<u64>)> {
    let object = match raw {
        Raw::Object(entries) => entries,
        _ => bail!(
            "LSP config {}: top level must be a JSON object",
            source.label
        ),
    };
    if let Some((_, servers)) = object.iter().find(|(key, _)| key == "servers") {
        let Raw::Object(servers) = servers else {
            bail!(
                "LSP config {}: field `servers` must be an object",
                source.label
            );
        };
        let mut result = Vec::new();
        for (name, value) in servers {
            ensure!(
                matches!(value, Raw::Object(_)),
                "LSP config {}: field `servers.{name}` must be an object",
                source.label
            );
            result.push((name.clone(), value.clone()));
        }
        let idle = match object.iter().find(|(key, _)| key == "idleTimeoutMs") {
            Some((_, value)) => Some(expect_u64(value, "idleTimeoutMs", source.path())?),
            None => None,
        };
        return Ok((result, idle));
    }
    let mut result = Vec::new();
    let mut idle = None;
    for (name, value) in object {
        if name == "idleTimeoutMs" {
            idle = Some(expect_u64(value, "idleTimeoutMs", source.path())?);
            continue;
        }
        ensure!(
            matches!(value, Raw::Object(_)),
            "LSP config {}: server `{name}` must be an object",
            source.label
        );
        result.push((name.clone(), value.clone()));
    }
    Ok((result, idle))
}

fn merge_fields(fields: &mut Vec<(String, Raw)>, override_value: &Raw) {
    for (key, value) in object_fields(override_value) {
        match fields.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, slot)) => *slot = value,
            None => fields.push((key, value)),
        }
    }
}

fn object_fields(value: &Raw) -> Vec<(String, Raw)> {
    match value {
        Raw::Object(entries) => entries.clone(),
        _ => Vec::new(),
    }
}

/// Validate field types of one server definition inside a layer (which may be only a partial override at this point).
fn validate_server_shape(name: &str, value: &Raw, source: &Source) -> Result<()> {
    let Raw::Object(fields) = value else {
        bail!(
            "LSP config {}: server `{name}` must be an object",
            source.label
        );
    };
    for (key, field) in fields {
        let path = format!("servers.{name}.{key}");
        match key.as_str() {
            "command" | "languageId" => {
                let text = expect_string(field, &path, source.path())?;
                ensure!(
                    !text.trim().is_empty(),
                    "LSP config {}: field `{path}` must not be empty",
                    source.label
                );
            }
            "args" | "fileTypes" | "rootMarkers" => {
                let items = expect_string_array(field, &path, source.path())?;
                ensure!(
                    key == "args" || !items.is_empty(),
                    "LSP config {}: field `{path}` must not be empty",
                    source.label
                );
                ensure!(
                    items.iter().all(|item| !item.trim().is_empty()),
                    "LSP config {}: field `{path}` must contain non-empty strings",
                    source.label
                );
            }
            "initOptions" | "settings" | "capabilities" | "workspaceReadyTimings" => {
                ensure!(
                    matches!(field, Raw::Object(_)),
                    "LSP config {}: field `{path}` must be an object",
                    source.label
                );
                if key == "workspaceReadyTimings" {
                    let Raw::Object(entries) = field else {
                        unreachable!();
                    };
                    for (timing, value) in entries {
                        if READY_FIELDS.contains(&timing.as_str()) {
                            expect_u64(value, &format!("{path}.{timing}"), source.path())?;
                        }
                    }
                }
            }
            "disabled" | "isLinter" => {
                ensure!(
                    matches!(field, Raw::Bool(_)),
                    "LSP config {}: field `{path}` must be a boolean",
                    source.label
                );
            }
            "warmupTimeoutMs" => {
                let value = expect_u64(field, &path, source.path())?;
                ensure!(
                    value > 0,
                    "LSP config {}: field `{path}` must be greater than zero",
                    source.label
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn finish_definition(
    name: &str,
    fields: &[(String, Raw)],
    source: &str,
) -> Result<ServerDefinition> {
    let field = |key: &str| fields.iter().find(|(name, _)| name == key).map(|(_, v)| v);
    let path_prefix = format!("servers.{name}");
    let require = |key: &str| -> Result<&Raw> {
        field(key).with_context(|| {
            format!("LSP config {source}: missing required field `{path_prefix}.{key}`")
        })
    };
    let command = expect_string(
        require("command")?,
        &format!("{path_prefix}.command"),
        Path::new(source),
    )?;
    let args = match field("args") {
        Some(value) => {
            expect_string_array(value, &format!("{path_prefix}.args"), Path::new(source))?
        }
        None => Vec::new(),
    };
    let file_types = expect_string_array(
        require("fileTypes")?,
        &format!("{path_prefix}.fileTypes"),
        Path::new(source),
    )?;
    let root_markers = expect_string_array(
        require("rootMarkers")?,
        &format!("{path_prefix}.rootMarkers"),
        Path::new(source),
    )?;
    let language_id = match field("languageId") {
        Some(value) => Some(expect_string(
            value,
            &format!("{path_prefix}.languageId"),
            Path::new(source),
        )?),
        None => None,
    };
    let mut ready = ReadyTimings::default();
    if let Some(Raw::Object(entries)) = field("workspaceReadyTimings") {
        for (key, value) in entries {
            let slot = match key.as_str() {
                "timeoutMs" => &mut ready.timeout_ms,
                "pollMs" => &mut ready.poll_ms,
                "settleMs" => &mut ready.settle_ms,
                "statusRequestTimeoutMs" => &mut ready.status_request_timeout_ms,
                _ => continue,
            };
            *slot = value.as_u64();
        }
    }
    Ok(ServerDefinition {
        name: name.to_owned(),
        command,
        args,
        file_types,
        language_id,
        root_markers,
        init_options: field("initOptions").map_or_else(|| Value::Object(Map::new()), Raw::to_json),
        settings: field("settings").map_or_else(|| Value::Object(Map::new()), Raw::to_json),
        disabled: field("disabled").and_then(Raw::as_bool).unwrap_or(false),
        warmup_timeout_ms: field("warmupTimeoutMs").and_then(Raw::as_u64),
        is_linter: field("isLinter").and_then(Raw::as_bool).unwrap_or(false),
        _capabilities: field("capabilities")
            .map_or_else(|| Value::Object(Map::new()), Raw::to_json),
        ready,
        resolved_command: None,
    })
}

fn expect_string(value: &Raw, path: &str, source: &Path) -> Result<String> {
    match value {
        Raw::String(text) => Ok(text.clone()),
        _ => bail!(
            "LSP config {}: field `{path}` must be a string",
            source.display()
        ),
    }
}

fn expect_u64(value: &Raw, path: &str, source: &Path) -> Result<u64> {
    let parsed = match value {
        Raw::Number(number) => number.as_u64(),
        _ => None,
    };
    parsed.with_context(|| {
        format!(
            "LSP config {}: field `{path}` must be a non-negative integer",
            source.display()
        )
    })
}

fn expect_string_array(value: &Raw, path: &str, source: &Path) -> Result<Vec<String>> {
    let Raw::Array(items) = value else {
        bail!(
            "LSP config {}: field `{path}` must be an array of strings",
            source.display()
        );
    };
    let mut result = Vec::new();
    for item in items {
        match item {
            Raw::String(text) => result.push(text.clone()),
            _ => bail!(
                "LSP config {}: field `{path}` must be an array of strings",
                source.display()
            ),
        }
    }
    Ok(result)
}

// =============================================================================
// Strict JSON parsing (preserves key order and rejects duplicate keys)
// =============================================================================

#[derive(Clone, Debug)]
enum Raw {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Raw>),
    Object(Vec<(String, Raw)>),
}

impl Raw {
    fn get(&self, key: &str) -> Option<&Raw> {
        match self {
            Raw::Object(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Raw::Bool(value) => Some(*value),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            Raw::Number(number) => number.as_u64(),
            _ => None,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Raw::Null => Value::Null,
            Raw::Bool(value) => Value::Bool(*value),
            Raw::Number(number) => Value::Number(number.clone()),
            Raw::String(text) => Value::String(text.clone()),
            Raw::Array(items) => Value::Array(items.iter().map(Raw::to_json).collect()),
            Raw::Object(entries) => {
                let mut map = Map::new();
                for (key, value) in entries {
                    map.insert(key.clone(), value.to_json());
                }
                Value::Object(map)
            }
        }
    }
}

impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawVisitor;

        impl<'de> Visitor<'de> for RawVisitor {
            type Value = Raw;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Raw, E> {
                Ok(Raw::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Raw, E> {
                Ok(Raw::Number(Number::from(value)))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Raw, E> {
                Ok(Raw::Number(Number::from(value)))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Raw, E> {
                Number::from_f64(value)
                    .map(Raw::Number)
                    .ok_or_else(|| E::custom("non-finite number"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Raw, E> {
                Ok(Raw::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Raw, E> {
                Ok(Raw::String(value))
            }

            fn visit_none<E>(self) -> Result<Raw, E> {
                Ok(Raw::Null)
            }

            fn visit_unit<E>(self) -> Result<Raw, E> {
                Ok(Raw::Null)
            }

            fn visit_some<D2>(self, deserializer: D2) -> Result<Raw, D2::Error>
            where
                D2: Deserializer<'de>,
            {
                Raw::deserialize(deserializer)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Raw, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut items = Vec::new();
                while let Some(value) = seq.next_element::<Raw>()? {
                    items.push(value);
                }
                Ok(Raw::Array(items))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Raw, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries: Vec<(String, Raw)> = Vec::new();
                let mut seen = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(de::Error::custom(format!("duplicate key `{key}`")));
                    }
                    entries.push((key, map.next_value::<Raw>()?));
                }
                Ok(Raw::Object(entries))
            }
        }

        deserializer.deserialize_any(RawVisitor)
    }
}

/// Parse JSON text and reject duplicate keys; error messages keep only the path and category.
fn parse_raw(text: &str, path: &Path) -> Result<Raw> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let parsed: Raw = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        anyhow::anyhow!(
            "LSP config {}: invalid JSON at `{}`: {}",
            path.display(),
            error.path(),
            classify_json_error(error.inner())
        )
    })?;
    deserializer.end().map_err(|error| {
        anyhow::anyhow!(
            "LSP config {}: invalid JSON: {}",
            path.display(),
            classify_json_error(&error)
        )
    })?;
    Ok(parsed)
}

/// A serde_json error location never contains configuration values; strip any original text fragment that may appear.
fn classify_json_error(error: &serde_json::Error) -> String {
    match error.classify() {
        serde_json::error::Category::Io => "read error".to_owned(),
        serde_json::error::Category::Syntax => "syntax error".to_owned(),
        // Only this module's Raw visitor produces Data-category errors (duplicate keys, non-finite numbers),
        // and the text holds only key names, no configuration values.
        serde_json::error::Category::Data => error.to_string(),
        serde_json::error::Category::Eof => "unexpected end of input".to_owned(),
    }
}

// =============================================================================
// Local discovery
// =============================================================================

/// Filter to servers whose root marker matched and whose executable can be resolved.
pub(crate) fn discover(cwd: &Path, definitions: &[ServerDefinition]) -> Vec<ServerDefinition> {
    let mut servers = Vec::new();
    for definition in definitions {
        if definition.disabled {
            continue;
        }
        if !has_root_markers(cwd, &definition.root_markers) {
            continue;
        }
        let Some(resolved) = resolve_command(&definition.command, cwd) else {
            continue;
        };
        let mut found = definition.clone();
        found.resolved_command = Some(resolved);
        servers.push(found);
    }
    select_typescript_server(&mut servers);
    servers
}

pub(crate) fn has_root_markers(cwd: &Path, markers: &[String]) -> bool {
    let mut entries: Option<Vec<String>> = None;
    for marker in markers {
        if marker.contains('*') {
            let listing = entries.get_or_insert_with(|| {
                fs::read_dir(cwd)
                    .map(|dir| {
                        dir.filter_map(|entry| entry.ok())
                            .filter_map(|entry| entry.file_name().into_string().ok())
                            .collect()
                    })
                    .unwrap_or_default()
            });
            let pattern = match globset::GlobBuilder::new(marker)
                .literal_separator(true)
                .build()
            {
                Ok(glob) => glob.compile_matcher(),
                Err(_) => continue,
            };
            if listing.iter().any(|entry| pattern.is_match(entry)) {
                return true;
            }
            continue;
        }
        if fs::symlink_metadata(cwd.join(marker)).is_ok() {
            return true;
        }
    }
    false
}

fn is_executable_file(path: &Path) -> bool {
    // `metadata` follows symlinks: a launcher symlink pointing to an executable regular file is usable, but
    // this only relaxes "resolving the executable", never any write restriction.
    match fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn local_bin_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut check = |markers: &[&str], bin: &str| {
        let markers: Vec<String> = markers.iter().map(|marker| (*marker).to_owned()).collect();
        if has_root_markers(cwd, &markers) {
            dirs.push(cwd.join(bin));
        }
    };
    check(NODE_ROOT_MARKERS, "node_modules/.bin");
    for bin in [
        ".venv/bin",
        ".venv/Scripts",
        "venv/bin",
        "venv/Scripts",
        ".env/bin",
        ".env/Scripts",
    ] {
        check(PYTHON_ROOT_MARKERS, bin);
    }
    check(RUBY_ROOT_MARKERS, "vendor/bundle/bin");
    check(RUBY_ROOT_MARKERS, "bin");
    check(GO_ROOT_MARKERS, "bin");
    dirs
}

/// Resolve the server command: workspace-local bin directories first, then PATH.
pub(crate) fn resolve_command(command: &str, cwd: &Path) -> Option<PathBuf> {
    if command.contains('/') {
        let path = Path::new(command);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        return is_executable_file(&candidate).then_some(candidate);
    }
    for dir in local_bin_dirs(cwd) {
        let candidate = dir.join(command);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable_file(candidate))
}

/// Keep only one TypeScript server: when the package shipping `tsc` lacks `lib/tsserver.js`
/// (the TypeScript 7 native implementation) keep `typescript-native`, otherwise keep the wrapper.
fn select_typescript_server(servers: &mut Vec<ServerDefinition>) {
    let Some(native) = servers
        .iter()
        .find(|server| server.name == "typescript-native")
        .and_then(|server| server.resolved_command.clone())
    else {
        return;
    };
    let drop = if typescript_speaks_lsp(&native) {
        "typescript-language-server"
    } else {
        "typescript-native"
    };
    servers.retain(|server| server.name != drop);
}

fn typescript_speaks_lsp(tsc: &Path) -> bool {
    let real = fs::canonicalize(tsc).unwrap_or_else(|_| tsc.to_path_buf());
    let mut candidates = Vec::new();
    let real_parent = real.parent();
    if let Some(parent) = real_parent
        && parent.file_name() == Some(OsStr::new("bin"))
        && let Some(root) = parent.parent()
    {
        candidates.push(root.to_path_buf());
    }
    if let Some(bin_dir) = tsc.parent() {
        candidates.push(bin_dir.join("..").join("typescript"));
        candidates.push(bin_dir.join("node_modules").join("typescript"));
    }
    for dir in candidates {
        if dir.join("package.json").exists() {
            return !dir.join("lib").join("tsserver.js").exists();
        }
    }
    false
}

#[cfg(test)]
mod tests;
