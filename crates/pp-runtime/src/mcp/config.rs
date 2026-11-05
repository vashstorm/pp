use super::{RemoteConfig, ServerConfig, StdioConfig, TransportConfig, validate_config};
use anyhow::{Result, bail, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const INVALID: &str = "Invalid MCP config: check JSON syntax, duplicate keys, fields, and types";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryView {
    pub name: String,
    pub source: String,
    pub path: PathBuf,
    pub status: &'static str,
}
#[derive(Clone, Debug, Default)]
pub struct View {
    pub entries: Vec<EntryView>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone)]
pub(super) struct Entry {
    pub source: String,
    pub path: PathBuf,
    pub enabled: bool,
    pub config: Option<ServerConfig>,
    pub status: &'static str,
}
#[derive(Clone, Default)]
pub(super) struct Directory {
    pub path: PathBuf,
    pub entries: BTreeMap<String, Entry>,
    pub diagnostics: Vec<String>,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Native {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
    #[serde(default)]
    mcp_servers: BTreeMap<String, Value>,
    #[serde(default)]
    enabled_servers: Vec<String>,
    #[serde(default)]
    disabled_servers: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    #[serde(default, rename = "type")]
    transport: Option<String>,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    #[serde(default = "enabled")]
    #[serde(rename = "enabled")]
    _enabled: bool,
    #[serde(default = "timeout")]
    timeout: u64,
}
fn enabled() -> bool {
    true
}
fn timeout() -> u64 {
    30_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteDefinition {
    #[serde(rename = "type")]
    transport: String,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    env_headers: BTreeMap<String, String>,
    #[serde(default = "enabled", rename = "enabled")]
    _enabled: bool,
    #[serde(default = "timeout")]
    timeout: u64,
}

fn remote_config(
    url: &str,
    literal: &BTreeMap<String, String>,
    environment: &BTreeMap<String, String>,
    mut lookup: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<RemoteConfig> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let url = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!(INVALID))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        INVALID
    );
    let mut headers = HeaderMap::new();
    let name = |key: &str| -> Result<HeaderName> {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| anyhow::anyhow!(INVALID))?;
        ensure!(
            !matches!(
                name.as_str(),
                "accept"
                    | "content-type"
                    | "mcp-session-id"
                    | "mcp-protocol-version"
                    | "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "last-event-id"
                    | "accept-encoding"
                    | "te"
                    | "trailer"
                    | "upgrade"
            ),
            INVALID
        );
        Ok(name)
    };
    for (key, value) in literal {
        let key = name(key)?;
        ensure!(!headers.contains_key(&key) && !value.is_empty(), INVALID);
        let mut value = HeaderValue::from_str(value).map_err(|_| anyhow::anyhow!(INVALID))?;
        value.set_sensitive(true);
        headers.insert(key, value);
    }
    let mut seen = std::collections::BTreeSet::new();
    for (key, variable) in environment {
        let key = name(key)?;
        ensure!(
            seen.insert(key.as_str().to_owned())
                && !variable.is_empty()
                && !variable.contains(['=', '\0']),
            INVALID
        );
        if let Some(value) = lookup(variable)? {
            ensure!(!value.is_empty(), INVALID);
            let mut value = HeaderValue::from_str(&value).map_err(|_| anyhow::anyhow!(INVALID))?;
            value.set_sensitive(true);
            headers.insert(key, value);
        } else {
            ensure!(headers.contains_key(&key), INVALID);
        }
    }
    Ok(RemoteConfig { url, headers })
}

fn json_value(text: &str) -> Result<Value> {
    // JSON 负责语法边界，YAML 的映射解析负责拒绝所有层级的重复键。
    let _: Value = serde_json::from_str(text).map_err(|_| anyhow::anyhow!(INVALID))?;
    pp_config::yaml::parse(text, "MCP").map_err(|_| anyhow::anyhow!(INVALID))
}
fn read_native(path: &Path) -> Result<Native> {
    match fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_value(json_value(&text)?).map_err(|_| anyhow::anyhow!(INVALID))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Native::default()),
        Err(_) => bail!("cannot read MCP config"),
    }
}

impl Directory {
    pub fn load(path: &Path) -> Result<Self> {
        #[cfg(not(test))]
        let home = std::env::var_os("HOME").map(PathBuf::from);
        // 单元测试不得读取开发者的真实外部配置；发现测试显式提供隔离 HOME。
        #[cfg(test)]
        let home: Option<PathBuf> = None;
        Self::load_at(path, home.as_deref())
    }
    fn load_at(path: &Path, home: Option<&Path>) -> Result<Self> {
        let native = read_native(path)?;
        let mut dir = Self {
            path: path.into(),
            ..Self::default()
        };
        dir.add(native.mcp_servers, "pp", path, false);
        if let Some(home) = home {
            for relative in [
                ".claude.json",
                ".claude/mcp.json",
                ".claude/.mcp.json",
                ".codex/config.toml",
            ] {
                let file = home.join(relative);
                let text = match fs::read_to_string(&file) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => {
                        dir.diagnostics
                            .push(format!("cannot read MCP source: {relative}"));
                        continue;
                    }
                };
                let codex = relative.ends_with(".toml");
                let parsed = if codex {
                    toml::from_str::<Value>(&text).map_err(|_| anyhow::anyhow!(INVALID))
                } else {
                    json_value(&text)
                };
                match parsed {
                    Ok(value) => {
                        let key = if codex { "mcp_servers" } else { "mcpServers" };
                        if let Some(servers) = value.get(key) {
                            if let Some(map) = servers.as_object() {
                                dir.add(
                                    map.clone().into_iter().collect(),
                                    if codex { "Codex" } else { "Claude Code" },
                                    &file,
                                    codex,
                                );
                            } else {
                                dir.diagnostics
                                    .push(format!("invalid MCP source field: {relative}"));
                            }
                        }
                    }
                    Err(_) => dir
                        .diagnostics
                        .push(format!("invalid MCP source format: {relative}")),
                }
            }
        }
        ensure!(
            dir.entries.len() <= 32,
            "MCP server count exceeds the limit of 32"
        );
        for (name, entry) in &mut dir.entries {
            if native.enabled_servers.contains(name) {
                entry.enabled = true;
            }
            if native.disabled_servers.contains(name) {
                entry.enabled = false;
            }
        }
        Ok(dir)
    }
    fn add(&mut self, servers: BTreeMap<String, Value>, source: &str, path: &Path, codex: bool) {
        for (name, mut value) in servers {
            if self.entries.contains_key(&name) {
                continue;
            }
            let original_enabled = value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let parsed = (|| -> Result<ServerConfig> {
                if codex {
                    value = normalize_codex(&value)?;
                }
                if source != "pp"
                    && (value.get("url").is_some()
                        || value
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|t| t != "stdio"))
                {
                    bail!("unsupported");
                }
                let remote = value
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| matches!(t, "http" | "sse"));
                let config = if remote {
                    let def: RemoteDefinition =
                        serde_json::from_value(value).map_err(|_| anyhow::anyhow!(INVALID))?;
                    let config = remote_config(&def.url, &def.headers, &def.env_headers, |name| {
                        match std::env::var(name) {
                            Ok(value) => Ok(Some(value)),
                            Err(std::env::VarError::NotPresent) => Ok(None),
                            Err(_) => bail!("invalid MCP header environment"),
                        }
                    })?;
                    ServerConfig {
                        transport: if def.transport == "http" {
                            TransportConfig::Http(config)
                        } else {
                            TransportConfig::Sse(config)
                        },
                        timeout_seconds: 30,
                        timeout_millis: Some(def.timeout),
                    }
                } else {
                    let def: Definition =
                        serde_json::from_value(value).map_err(|_| anyhow::anyhow!(INVALID))?;
                    ensure!(
                        def.transport.as_deref().is_none_or(|t| t == "stdio"),
                        "unsupported"
                    );
                    let cwd = def.cwd.map(|cwd| {
                        if cwd.is_absolute() {
                            cwd
                        } else {
                            path.parent().unwrap_or(Path::new(".")).join(cwd)
                        }
                    });
                    ServerConfig {
                        transport: TransportConfig::Stdio(StdioConfig {
                            command: def.command,
                            args: def.args,
                            env: def.env,
                            cwd,
                        }),
                        timeout_seconds: 30,
                        timeout_millis: Some(def.timeout),
                    }
                };
                validate_config(&BTreeMap::from([(name.clone(), config.clone())]))?;
                Ok(config)
            })();
            self.entries.insert(
                name,
                Entry {
                    source: source.into(),
                    path: path.into(),
                    enabled: original_enabled,
                    status: if parsed.is_ok() {
                        "not connected"
                    } else {
                        "unsupported / invalid config"
                    },
                    config: parsed.ok(),
                },
            );
        }
    }
    pub fn save_enabled(&self, name: &str, enabled: bool, token: &CancellationToken) -> Result<()> {
        ensure!(self.entries.contains_key(name), "unknown MCP server");
        let parent = self.path.parent().ok_or_else(|| anyhow::anyhow!(INVALID))?;
        fs::create_dir_all(parent)
            .map_err(|_| anyhow::anyhow!("cannot create MCP config directory"))?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(parent.join("mcp.lock"))
            .map_err(|_| anyhow::anyhow!("cannot open MCP config lock"))?;
        ensure!(
            lock.metadata().is_ok_and(|m| m.is_file()),
            "MCP config lock must be a regular file"
        );
        let start = Instant::now();
        loop {
            cancellation::check(token)?;
            match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::INTR | rustix::io::Errno::WOULDBLOCK) => {}
                Err(_) => bail!("cannot acquire MCP config lock"),
            }
            ensure!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting for the MCP config lock"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        // 持锁后重新读取，保留其他进程刚保存的服务和启停设置。
        let mut config = read_native(&self.path)?;
        config.enabled_servers.retain(|n| n != name);
        config.disabled_servers.retain(|n| n != name);
        if enabled {
            config.enabled_servers.push(name.into());
        } else {
            config.disabled_servers.push(name.into());
        }
        let data = serde_json::to_vec_pretty(&config).map_err(|_| anyhow::anyhow!(INVALID))?;
        let save = || -> std::io::Result<()> {
            let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
            tmp.as_file()
                .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            tmp.write_all(&data)?;
            tmp.write_all(b"\n")?;
            tmp.as_file().sync_all()?;
            tmp.persist(&self.path).map_err(|e| e.error)?;
            Ok(())
        };
        cancellation::check(token)?;
        save().map_err(|_| anyhow::anyhow!("failed to save MCP config"))?;
        Ok(())
    }
}

fn normalize_codex(value: &Value) -> Result<Value> {
    let map = value.as_object().ok_or_else(|| anyhow::anyhow!(INVALID))?;
    // 未支持的限制、授权或传输选项不能被静默丢弃后启动。
    let allowed = [
        "command",
        "args",
        "env",
        "env_vars",
        "cwd",
        "enabled",
        "tool_timeout_sec",
        "startup_timeout_sec",
    ];
    ensure!(
        map.keys().all(|key| allowed.contains(&key.as_str())),
        "unsupported"
    );
    let mut result = map.clone();
    if let Some(vars) = result.remove("env_vars") {
        let names: Vec<String> =
            serde_json::from_value(vars).map_err(|_| anyhow::anyhow!(INVALID))?;
        let mut env: BTreeMap<String, String> =
            serde_json::from_value(result.remove("env").unwrap_or(json!({})))
                .map_err(|_| anyhow::anyhow!(INVALID))?;
        for name in names {
            if let Ok(value) = std::env::var(&name) {
                env.insert(name, value);
            }
        }
        result.insert("env".into(), json!(env));
    }
    // 当前实现统一请求截止时间；显式不同的启动/工具超时不降级处理。
    let tool = result.remove("tool_timeout_sec");
    let startup = result.remove("startup_timeout_sec");
    ensure!(
        tool.is_none() || startup.is_none() || tool == startup,
        "unsupported"
    );
    if let Some(seconds) = tool.or(startup) {
        let millis = seconds
            .as_f64()
            .filter(|v| v.is_finite() && *v > 0.0 && *v <= (u64::MAX / 1000) as f64)
            .map(|s| (s * 1000.0).ceil() as u64)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        result.insert("timeout".into(), json!(millis));
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stdio(entry: &Entry) -> &StdioConfig {
        let TransportConfig::Stdio(config) = &entry.config.as_ref().unwrap().transport else {
            panic!("预期 stdio")
        };
        config
    }
    fn put(path: &Path, value: Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value.to_string()).unwrap();
    }
    #[test]
    fn mcp_discovery_precedence_overrides_and_user_only() {
        let home = tempfile::tempdir().unwrap();
        let pp = home.path().join(".pp/agent/mcp.json");
        put(
            &pp,
            json!({"mcpServers":{"same":{"command":"pp"}},"enabledServers":["off"],"disabledServers":["same","off"]}),
        );
        put(
            &home.path().join(".claude.json"),
            json!({"projects":{"ignored":{"mcpServers":{"project":{"command":"bad"}}}},"mcpServers":{"same":{"command":"claude"},"off":{"command":"off","enabled":false},"claude":{"command":"claude"}}}),
        );
        put(
            &home.path().join(".claude/mcp.json"),
            json!({"mcpServers":{"claude":{"command":"shadowed"},"alternate":{"command":"alternate"}}}),
        );
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        fs::write(home.path().join(".codex/config.toml"), "[mcp_servers.same]\ncommand='codex'\n[mcp_servers.codex]\ncommand='codex'\ncwd='server'\ntool_timeout_sec=2\n").unwrap();
        let dir = Directory::load_at(&pp, Some(home.path())).unwrap();
        assert_eq!(dir.entries.len(), 5);
        assert_eq!(stdio(&dir.entries["same"]).command, "pp");
        assert_eq!(stdio(&dir.entries["claude"]).command, "claude");
        assert!(!dir.entries["same"].enabled);
        assert!(!dir.entries["off"].enabled);
        let codex = dir.entries["codex"].config.as_ref().unwrap();
        assert_eq!(codex.timeout(), Duration::from_secs(2));
        assert_eq!(
            stdio(&dir.entries["codex"]).cwd.as_deref(),
            Some(home.path().join(".codex/server").as_path())
        );
    }
    #[test]
    fn mcp_remote_headers_environment_precedence_fallback_and_validation() {
        let url = "https://mcp.context7.com/mcp";
        let literal = BTreeMap::from([("Context7-API-Key".into(), "literal-secret".into())]);
        let environment = BTreeMap::from([("context7-api-key".into(), "PP_TEST_KEY".into())]);
        let config = remote_config(url, &literal, &environment, |_| {
            Ok(Some("env-secret".into()))
        })
        .unwrap();
        assert_eq!(config.headers["context7-api-key"], "env-secret");
        let config = remote_config(url, &literal, &environment, |_| Ok(None)).unwrap();
        assert_eq!(config.headers["Context7-API-Key"], "literal-secret");
        assert!(remote_config(url, &literal, &environment, |_| Ok(Some(String::new()))).is_err());
        assert!(remote_config(url, &BTreeMap::new(), &environment, |_| Ok(None)).is_err());
        assert!(
            remote_config(url, &BTreeMap::new(), &environment, |_| Ok(Some(
                "only-env".into()
            )))
            .is_ok()
        );
        assert!(remote_config(url, &literal, &BTreeMap::new(), |_| unreachable!()).is_ok());
        for key in [
            "bad header",
            "Accept",
            "mcp-session-id",
            "MCP-Protocol-Version",
            "Host",
            "Connection",
            "Content-Length",
        ] {
            let headers = BTreeMap::from([(key.into(), "secret".into())]);
            assert!(remote_config(url, &headers, &BTreeMap::new(), |_| unreachable!()).is_err());
        }
        let duplicate = BTreeMap::from([
            ("X-Key".into(), "one".into()),
            ("x-key".into(), "two".into()),
        ]);
        assert!(remote_config(url, &duplicate, &BTreeMap::new(), |_| unreachable!()).is_err());
        assert!(
            remote_config(url, &BTreeMap::new(), &duplicate, |_| Ok(Some(
                "key".into()
            )))
            .is_err()
        );
        for value in ["", "key\r\nInjected: secret"] {
            assert!(
                remote_config(
                    url,
                    &BTreeMap::from([("X-Key".into(), value.into())]),
                    &BTreeMap::new(),
                    |_| unreachable!()
                )
                .is_err()
            );
        }
        for url in [
            "file:///secret",
            "https://user:secret@example.com/mcp",
            "https://example.com/mcp#secret",
            "secret",
        ] {
            assert!(remote_config(url, &literal, &BTreeMap::new(), |_| unreachable!()).is_err());
        }
    }

    #[test]
    fn mcp_remote_config_only_native_strict_fields_and_preserved_save() {
        let home = tempfile::tempdir().unwrap();
        let pp = home.path().join("mcp.json");
        let remote = json!({"type":"http","url":"http://127.0.0.1:1/mcp","headers":{"X-Key":"literal"},"env_headers":{"x-key":"PP_TEST_MCP_ENV_MISSING_536F"}});
        put(
            &pp,
            json!({"mcpServers":{"remote":remote,"sse":{"type":"sse","url":"https://example.com/sse"}}}),
        );
        put(
            &home.path().join(".claude.json"),
            json!({"mcpServers":{"foreign":remote}}),
        );
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        fs::write(
            home.path().join(".codex/config.toml"),
            "[mcp_servers.codex]\nurl='https://example.com/mcp'\n",
        )
        .unwrap();
        let dir = Directory::load_at(&pp, Some(home.path())).unwrap();
        assert!(matches!(
            dir.entries["remote"].config.as_ref().unwrap().transport,
            TransportConfig::Http(_)
        ));
        assert!(matches!(
            dir.entries["sse"].config.as_ref().unwrap().transport,
            TransportConfig::Sse(_)
        ));
        assert!(dir.entries["foreign"].config.is_none());
        assert!(dir.entries["codex"].config.is_none());
        dir.save_enabled("remote", false, &CancellationToken::new())
            .unwrap();
        assert_eq!(read_native(&pp).unwrap().mcp_servers["remote"], remote);
        for field in [
            json!({"command":"x"}),
            json!({"args":[]}),
            json!({"env":{}}),
            json!({"cwd":"."}),
            json!({"timeout":0}),
            json!({"timeout":null}),
            json!({"unknown":true}),
        ] {
            let mut bad = remote.clone();
            bad.as_object_mut()
                .unwrap()
                .extend(field.as_object().unwrap().clone());
            put(&pp, json!({"mcpServers":{"bad":bad}}));
            assert!(
                Directory::load_at(&pp, None).unwrap().entries["bad"]
                    .config
                    .is_none()
            );
        }
        put(
            &pp,
            json!({"mcpServers":{"no_type":{"url":"https://example.com/mcp"}}}),
        );
        assert!(
            Directory::load_at(&pp, None).unwrap().entries["no_type"]
                .config
                .is_none()
        );
    }

    #[test]
    fn mcp_config_strict_json_and_redacted_invalid_entries() {
        let home = tempfile::tempdir().unwrap();
        let pp = home.path().join("mcp.json");
        for text in [
            "{mcpServers:{}}",
            r#"{"mcpServers":{},"unknown":"secret-value"}"#,
            r#"{"mcpServers":{"a":{"command":"x","command":"secret-value"}}}"#,
            r#"{"enabledServers":null}"#,
        ] {
            fs::write(&pp, text).unwrap();
            let error = Directory::load_at(&pp, None).err().unwrap();
            assert!(!error.to_string().contains("secret-value"));
        }
        put(
            &pp,
            json!({"mcpServers":{"good":{"command":"x","timeout":1},"bad":{"command":"x","timeout":0},"unknown":{"command":"secret-value","extra":"secret-value"},"remote":{"type":"http","url":"secret-value"}}}),
        );
        let dir = Directory::load_at(&pp, None).unwrap();
        assert!(dir.entries["good"].config.is_some());
        for name in ["bad", "unknown", "remote"] {
            assert!(dir.entries[name].config.is_none());
        }
        assert!(dir.diagnostics.is_empty());
    }
    #[test]
    fn mcp_save_enabled_locks_rereads_preserves_definitions_and_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let pp = home.path().join("mcp.json");
        put(
            &pp,
            json!({"mcpServers":{"a":{"command":"a"},"b":{"command":"b","enabled":false}}}),
        );
        let dir = Directory::load_at(&pp, None).unwrap();
        let another = dir.clone();
        let first = std::thread::spawn(move || {
            dir.save_enabled("a", false, &CancellationToken::new())
                .unwrap()
        });
        let second = std::thread::spawn(move || {
            another
                .save_enabled("b", true, &CancellationToken::new())
                .unwrap()
        });
        first.join().unwrap();
        second.join().unwrap();
        let config = read_native(&pp).unwrap();
        assert_eq!(config.disabled_servers, ["a"]);
        assert_eq!(config.enabled_servers, ["b"]);
        assert_eq!(config.mcp_servers["a"]["command"], "a");
        assert_eq!(
            fs::metadata(&pp).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let dir = Directory::load_at(&pp, None).unwrap();
        fs::write(&pp, "broken secret-value").unwrap();
        assert!(
            dir.save_enabled("a", true, &CancellationToken::new())
                .is_err()
        );
        assert_eq!(fs::read_to_string(&pp).unwrap(), "broken secret-value");
    }
    #[test]
    fn mcp_foreign_unsupported_and_malformed_sources_stay_isolated() {
        let home = tempfile::tempdir().unwrap();
        let pp = home.path().join("mcp.json");
        put(
            &home.path().join(".claude.json"),
            json!({"mcpServers":{"remote":{"type":"http","url":"secret-url"}}}),
        );
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        fs::write(home.path().join(".codex/config.toml"), "secret-broken").unwrap();
        let dir = Directory::load_at(&pp, Some(home.path())).unwrap();
        assert!(dir.entries["remote"].config.is_none());
        assert_eq!(dir.diagnostics.len(), 1);
        assert!(!dir.diagnostics[0].contains("secret"));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::mcp::{Action, Registry};
    #[test]
    fn mcp_plan_management_never_spawns_and_cleanup_tracks_later_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let log = dir.path().join("server-log");
        fs::write(&path, json!({"mcpServers":{"local":{"command":"bash","args":[format!("{}/../../tests/fixtures/mcp_server.sh", env!("CARGO_MANIFEST_DIR"))],"env":{"MCP_LOG":log.to_str().unwrap(),"MCP_VALUE":"test"}}}}).to_string()).unwrap();
        let token = CancellationToken::new();
        let mut registry = Registry::load(&path, dir.path(), &token, false).unwrap();
        let cleanup = registry.cleanup().unwrap();
        registry
            .manage(&Action::Disable("local".into()), dir.path(), &token, false)
            .unwrap();
        registry
            .manage(&Action::Enable("local".into()), dir.path(), &token, false)
            .unwrap();
        assert!(!log.exists());
        assert!(registry.definitions().is_empty());
        assert!(
            registry
                .manage(
                    &Action::Reconnect("local".into()),
                    dir.path(),
                    &token,
                    false
                )
                .is_err()
        );
        registry
            .manage(&Action::Reconnect("local".into()), dir.path(), &token, true)
            .unwrap();
        assert!(log.exists());
        assert!(!registry.definitions().is_empty());
        // 退出句柄在连接前取得，仍然必须关闭后来创建的进程并禁止复活。
        drop(cleanup);
        assert!(
            registry.servers["local"]
                .connection
                .lock()
                .unwrap()
                .child
                .is_none()
        );
        registry
            .manage(&Action::Reconnect("local".into()), dir.path(), &token, true)
            .unwrap();
        assert!(registry.definitions().is_empty());
        assert!(registry.view(true).entries[0].status.starts_with("failed"));
    }
}
