use super::*;
use rusqlite::Connection;

#[path = "stats/gc_cases.rs"]
mod gc_cases;
#[path = "stats/request_cases.rs"]
mod request_cases;
#[path = "stats/server_cases.rs"]
mod server_cases;
#[path = "stats/usage_cases.rs"]
mod usage_cases;

pub(super) fn database(home: &std::path::Path) -> Connection {
    Connection::open(home.join(".pp/stats/stats.db")).unwrap()
}
/// 每个请求一行：首个请求有快照时为 `{request, skills}`，其余请求为 null。
pub(super) fn contexts(home: &std::path::Path) -> Vec<Value> {
    database(home)
        .prepare("SELECT c.request_json,c.skills_json FROM requests r LEFT JOIN request_contexts c ON c.request_id=r.id ORDER BY r.id")
        .unwrap()
        .query_map([], |row| {
            let request = row.get::<_, Option<String>>(0)?;
            let skills = row.get::<_, Option<String>>(1)?;
            Ok(request.map(|request| json!({
                "request": serde_json::from_str::<Value>(&request).unwrap(),
                "skills": serde_json::from_str::<Value>(skills.as_deref().unwrap()).unwrap(),
            })).unwrap_or(Value::Null))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
pub(super) fn rows(home: &std::path::Path) -> Vec<Value> {
    database(home).prepare("SELECT id,status,input_tokens,output_tokens,cached_tokens,ttft_ms,purpose,session_file,error_message,cache_write_tokens FROM requests ORDER BY id").unwrap().query_map([],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"status":r.get::<_,String>(1)?,"input":r.get::<_,Option<i64>>(2)?,"output":r.get::<_,Option<i64>>(3)?,"cache":r.get::<_,Option<i64>>(4)?,"ttft":r.get::<_,Option<f64>>(5)?,"purpose":r.get::<_,String>(6)?,"session_file":r.get::<_,Option<String>>(7)?,"error":r.get::<_,Option<String>>(8)?,"cache_write":r.get::<_,Option<i64>>(9)?}))).unwrap().collect::<Result<_,_>>().unwrap()
}

/// 模型在每个请求边界发出的工具调用：`request_tools` 只保存工具名与参数字符数。
pub(super) fn tools(home: &std::path::Path) -> Vec<Value> {
    database(home)
        .prepare("SELECT request_id,tool,args_chars FROM request_tools ORDER BY request_id,rowid")
        .unwrap()
        .query_map([], |r| {
            Ok(json!({
                "request":r.get::<_,i64>(0)?,
                "tool":r.get::<_,String>(1)?,
                "args":r.get::<_,i64>(2)?,
            }))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

pub(super) fn http(address: &str, path: &str, host: &str) -> (u16, String) {
    http_with_origin(address, path, host, None)
}

fn http_with_origin(address: &str, path: &str, host: &str, origin: Option<&str>) -> (u16, String) {
    let reply = raw_request(address, "GET", path, host, origin);
    (
        reply.status,
        String::from_utf8(reply.body).expect("文本资源必须是 UTF-8"),
    )
}

/// 原始回环响应：状态、headers 与未经解码的 body 字节。
struct RawReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawReply {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_default()
    }
}

/// 唯一的底层读取：文本 helper 与二进制／方法用例都走这里。
fn raw_request(
    address: &str,
    method: &str,
    path: &str,
    host: &str,
    origin: Option<&str>,
) -> RawReply {
    raw_request_body(address, method, path, host, origin, None)
}

fn raw_request_body(
    address: &str,
    method: &str,
    path: &str,
    host: &str,
    origin: Option<&str>,
    body: Option<(&str, &[u8])>,
) -> RawReply {
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.0\r\nHost: {host}\r\n").unwrap();
    if let Some(origin) = origin {
        write!(stream, "Origin: {origin}\r\n").unwrap();
    }
    if let Some((kind, body)) = body {
        write!(
            stream,
            "Content-Type: {kind}\r\nContent-Length: {}\r\n",
            body.len()
        )
        .unwrap();
    }
    write!(stream, "Connection: close\r\n\r\n").unwrap();
    if let Some((_, body)) = body {
        stream.write_all(body).unwrap();
    }
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP 响应必须带完整头部");
    let head = String::from_utf8(bytes[..split].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split_whitespace().nth(1).unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim_start().to_owned()))
        .collect();
    RawReply {
        status: status.parse().unwrap(),
        headers,
        body: bytes[split + 4..].to_vec(),
    }
}

pub(super) fn start_stats_server(dir: &std::path::Path) -> (Cli, String) {
    stats_endpoint(Cli::spawn(dir, &["stats", "--port", "0", "--no-open"], &[]))
}

fn stats_endpoint(mut cli: Cli) -> (Cli, String) {
    use std::io::BufRead;
    let stdout = cli.0.as_mut().unwrap().stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .unwrap();
        tx.send(line).unwrap();
    });
    let line = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("stats server did not start");
    let address = line
        .trim()
        .strip_prefix("pp stats: http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    (cli, address)
}

#[cfg(unix)]
pub(super) fn stats_browser_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let script = bin.join(program);
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$1\" > \"$HOME/opened-url\"\nexit \"${PP_STATS_OPEN_EXIT:-0}\"\n",
    )
    .unwrap();
    std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}
