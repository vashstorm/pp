//! `agent://`／`history://`／`proc://` URI 读取与结果文件解析。

use anyhow::{Context, Result, bail, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use serde_json::{Value, json};
use std::fs;

use super::{Binding, Run, lock};

impl Binding {
    pub(crate) fn uri(
        &self,
        op: &str,
        path: &str,
        content: Option<&str>,
        token: &CancellationToken,
    ) -> Result<Value> {
        let (scheme, rest) = path.split_once("://").context("invalid URI")?;
        let (id, suffix) = rest.split_once('/').unwrap_or((rest, ""));
        if scheme == "history" && id.is_empty() {
            let state = lock(&self.inner.state);
            let agents=state.disk.agents.values().map(|r|json!({"agent_id":r.id,"name":r.name,"parent":r.parent,"status":r.status,"output_uri":format!("agent://{}",r.id)})).collect::<Vec<_>>();
            return Ok(json!({"ok":true,"agents":agents}));
        }
        if scheme == "proc" {
            let state = lock(&self.inner.state);
            let run = state.disk.runs.get(id).context("unknown run")?;
            if op == "write" {
                ensure!(suffix == "kill", "only proc://run/kill is supported");
                if matches!(run.status.as_str(), "queued" | "running")
                    && let Some(live) = state.live.get(&run.agent_id)
                {
                    live.cancellation.cancel();
                }
                return Ok(json!({"ok":true}));
            }
            return Ok(json!({"ok":true,"run":run}));
        }
        ensure!(
            suffix.is_empty(),
            "agent URI does not support path suffixes"
        );
        let record = lock(&self.inner.state)
            .disk
            .agents
            .get(id)
            .cloned()
            .context("unknown agent")?;
        if op == "write" {
            ensure!(scheme == "agent", "history URI is read-only");
            return self.followup(
                &record,
                content.context("message must not be empty")?,
                token,
            );
        }
        if scheme == "history" {
            use std::io::{Read, Seek, SeekFrom};
            let path = self
                .inner
                .directory()
                .join(format!("{}.events.jsonl", record.id));
            let mut bytes = Vec::new();
            let mut truncated = false;
            match fs::File::open(&path) {
                Ok(mut file) => {
                    let size = file.metadata()?.len();
                    truncated = size > 65536;
                    if truncated {
                        file.seek(SeekFrom::Start(size - 65536))?;
                    }
                    file.take(65536).read_to_end(&mut bytes)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if truncated {
                if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                    bytes.drain(..=end);
                } else {
                    bytes.clear();
                }
            }
            if let Some(end) = bytes.iter().rposition(|b| *b == b'\n') {
                bytes.truncate(end + 1);
            } else {
                bytes.clear();
            }
            return Ok(
                json!({"ok":true,"history":String::from_utf8_lossy(&bytes),"truncated":truncated,"source_path":path}),
            );
        }
        bail!("unsupported subagent URI operation")
    }

    pub(crate) fn read_agent_result(
        &self,
        uri: &str,
        token: &CancellationToken,
    ) -> Result<(Value, Option<Value>)> {
        use std::io::{BufReader, Read};
        use std::os::unix::fs::OpenOptionsExt;

        const MAX_RESULT_BYTES: u64 = 16 * 1024 * 1024;

        let rest = uri.strip_prefix("agent://").context("invalid agent URI")?;
        let (id, pinned) = rest
            .split_once('/')
            .map_or((rest, None), |(id, run)| (id, Some(run)));
        let (metadata, published_id) = {
            let state = lock(&self.inner.state);
            let record = state.disk.agents.get(id).context("unknown agent")?;
            let sequence = |run: &&Run| {
                run.id
                    .strip_prefix('r')
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            let runs = || state.disk.runs.values().filter(|run| run.agent_id == id);
            let run = runs().max_by_key(sequence);
            let published = match pinned {
                Some(run_id) => Some(
                    runs()
                        .find(|run| run.id == run_id && run.result.is_some())
                        .context("unknown or unpublished agent run")?,
                ),
                None => runs()
                    .filter(|run| run.result.is_some())
                    .max_by_key(sequence),
            };
            (
                json!({"ok":true,"agent":record,"run":run,"published":published,"superseded":run.map(|r|&r.id)!=published.map(|r|&r.id),"history_uri":format!("history://{id}")}),
                published.map(|run| run.id.clone()),
            )
        };
        let Some(run_id) = published_id else {
            return Ok((metadata, None));
        };
        ensure!(
            run_id
                .strip_prefix('r')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|byte| byte.is_ascii_digit())),
            "invalid stored agent run id"
        );
        cancellation::check(token)?;
        let path = self.directory().join(format!("{run_id}.result.json"));
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(path);
        let result = match file {
            Ok(file) => {
                let info = file.metadata()?;
                ensure!(info.is_file(), "agent result must be a regular file");
                ensure!(
                    info.len() <= MAX_RESULT_BYTES,
                    "agent result exceeds 16 MiB"
                );
                let result: Value =
                    serde_json::from_reader(BufReader::new(file.take(MAX_RESULT_BYTES)))
                        .context("invalid agent result file")?;
                ensure!(result["ok"].is_boolean(), "invalid agent result file");
                result
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 旧登记表和中断恢复可能只有内联结果。已截断的预览不能冒充完整正文。
                let preview = &metadata["published"]["result"];
                ensure!(preview["truncated"] != true, "agent result file is missing");
                preview.clone()
            }
            Err(error) => return Err(error).context("opening agent result file"),
        };
        cancellation::check(token)?;
        Ok((metadata, Some(result)))
    }
}
