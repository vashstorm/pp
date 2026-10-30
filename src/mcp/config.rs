use super::{ServerConfig, validate_config};
use crate::cancellation::{self, CancellationToken};
use anyhow::{Result, bail, ensure};
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

const INVALID: &str = "MCP 配置无效：请检查 JSON 语法、重复键、字段及类型";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EntryView {
    pub name: String,
    pub source: String,
    pub path: PathBuf,
    pub status: &'static str,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct View {
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

fn json_value(text: &str) -> Result<Value> {
    // JSON 负责语法边界，YAML 的映射解析负责拒绝所有层级的重复键。
    let _: Value = serde_json::from_str(text).map_err(|_| anyhow::anyhow!(INVALID))?;
    crate::yaml::parse(text, "MCP").map_err(|_| anyhow::anyhow!(INVALID))
}
fn read_native(path: &Path) -> Result<Native> {
    match fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_value(json_value(&text)?).map_err(|_| anyhow::anyhow!(INVALID))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Native::default()),
        Err(_) => bail!("无法读取 MCP 配置"),
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
                            .push(format!("无法读取 MCP 来源：{relative}"));
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
                                    .push(format!("MCP 来源字段无效：{relative}"));
                            }
                        }
                    }
                    Err(_) => dir
                        .diagnostics
                        .push(format!("MCP 来源格式无效：{relative}")),
                }
            }
        }
        ensure!(dir.entries.len() <= 32, "MCP 服务数量超过上限 32");
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
                if value.get("url").is_some()
                    || value
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t != "stdio")
                {
                    bail!("unsupported");
                }
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
                let config = ServerConfig {
                    command: def.command,
                    args: def.args,
                    env: def.env,
                    timeout_seconds: 30,
                    timeout_millis: Some(def.timeout),
                    cwd,
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
        ensure!(self.entries.contains_key(name), "未知 MCP 服务");
        let parent = self.path.parent().ok_or_else(|| anyhow::anyhow!(INVALID))?;
        fs::create_dir_all(parent).map_err(|_| anyhow::anyhow!("无法创建 MCP 配置目录"))?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(parent.join("mcp.lock"))
            .map_err(|_| anyhow::anyhow!("无法打开 MCP 配置锁"))?;
        ensure!(
            lock.metadata().is_ok_and(|m| m.is_file()),
            "MCP 配置锁必须是普通文件"
        );
        let start = Instant::now();
        loop {
            cancellation::check(token)?;
            match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::INTR | rustix::io::Errno::WOULDBLOCK) => {}
                Err(_) => bail!("无法获取 MCP 配置锁"),
            }
            ensure!(
                start.elapsed() < Duration::from_secs(30),
                "等待 MCP 配置锁超时"
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
        save().map_err(|_| anyhow::anyhow!("保存 MCP 配置失败"))?;
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
        assert_eq!(dir.entries["same"].config.as_ref().unwrap().command, "pp");
        assert_eq!(
            dir.entries["claude"].config.as_ref().unwrap().command,
            "claude"
        );
        assert!(!dir.entries["same"].enabled);
        assert!(!dir.entries["off"].enabled);
        let codex = dir.entries["codex"].config.as_ref().unwrap();
        assert_eq!(codex.timeout(), Duration::from_secs(2));
        assert_eq!(
            codex.cwd.as_deref(),
            Some(home.path().join(".codex/server").as_path())
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
        fs::write(&path, json!({"mcpServers":{"local":{"command":"bash","args":[format!("{}/tests/fixtures/mcp_server.sh", env!("CARGO_MANIFEST_DIR"))],"env":{"MCP_LOG":log.to_str().unwrap(),"MCP_VALUE":"test"}}}}).to_string()).unwrap();
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
                .process
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
