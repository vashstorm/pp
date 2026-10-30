use std::{
    collections::VecDeque,
    sync::{LazyLock, Mutex},
};

const MAX_DETAIL_BYTES: usize = 64 * 1024;
static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 登记实际使用的凭据，包含其 JSON 转义形式，供所有线程的错误日志遮蔽。
pub(crate) fn register_secret(secret: &str) {
    if secret.is_empty() {
        return;
    }
    let mut secrets = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let escaped = serde_json::to_string(secret).expect("字符串可以序列化为 JSON");
    for value in [secret.to_owned(), escaped[1..escaped.len() - 1].to_owned()] {
        if !value.is_empty() && !secrets.contains(&value) {
            secrets.push(value);
        }
    }
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
}

/// 错误现场允许保留正文，但凭据不进入日志；先遮蔽再按 UTF-8 边界截断。
pub(crate) fn diagnostic_text(text: &str) -> String {
    static CREDENTIALS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r#"(?i)("(?:api[_-]?key|access[_-]?token|refresh[_-]?token|authorization|password|client[_-]?secret)"\s*:\s*)"(?:\\.|[^"\\])*""#,
    ).expect("固定的凭据字段正则有效")
    });
    static BEARER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)bearer[ \t]+[a-z0-9._~+/=-]+").expect("固定的 Bearer 正则有效")
    });
    let mut text = text.to_owned();
    for secret in SECRETS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        text = text.replace(secret, "[REDACTED]");
    }
    text = CREDENTIALS
        .replace_all(&text, "$1\"[REDACTED]\"")
        .into_owned();
    text = BEARER.replace_all(&text, "Bearer [REDACTED]").into_owned();
    bounded(text, MAX_DETAIL_BYTES)
}

fn bounded(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[truncated]");
    }
    text
}

/// 只在请求失败时落盘；有界保留最近的分片，成功请求不记录正文。
#[derive(Default)]
pub(crate) struct FailureContext {
    events: VecDeque<String>,
    bytes: usize,
    count: u64,
    dropped: u64,
}

impl FailureContext {
    pub(crate) fn push(&mut self, raw: &str) {
        self.count += 1;
        let text = bounded(diagnostic_text(raw), 16 * 1024);
        while self.bytes + text.len() > MAX_DETAIL_BYTES || self.events.len() >= 32 {
            let Some(old) = self.events.pop_front() else {
                break;
            };
            self.bytes -= old.len();
            self.dropped += 1;
        }
        self.bytes += text.len();
        self.events.push_back(text);
    }

    pub(crate) fn log<T>(
        &self,
        api: &str,
        model: &str,
        result: &anyhow::Result<T>,
        elapsed_ms: u64,
    ) {
        if let Err(error) = result
            && !error.is::<crate::cancellation::Cancelled>()
        {
            tracing::error!(
                event = "ai_request_failed", pid = std::process::id(), api, model,
                elapsed_ms, error = diagnostic_text(&format!("{error:#}")),
                received_events = self.count, dropped_events = self.dropped,
                recent_events = ?self.events,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_redact_credentials_and_bound_utf8() {
        register_secret("diagnostic-test-key-42");
        let text = diagnostic_text(
            r#"{"message":"diagnostic-test-key-42", "access_token":"other-token", "password":"hidden", "header":"Bearer abc.def"}"#,
        );
        for secret in ["diagnostic-test-key-42", "other-token", "hidden", "abc.def"] {
            assert!(!text.contains(secret), "{text}");
        }
        let text = diagnostic_text(&"中".repeat(MAX_DETAIL_BYTES));
        assert!(text.ends_with("[truncated]"));
        assert!(text.len() <= MAX_DETAIL_BYTES + 12);
    }

    #[test]
    fn diagnostics_recent_events_are_bounded_and_count_drops() {
        let mut context = FailureContext::default();
        for _ in 0..100 {
            context.push(&"x".repeat(20 * 1024));
        }
        assert_eq!(context.count, 100);
        assert!(context.dropped > 0);
        assert!(context.bytes <= MAX_DETAIL_BYTES);
        assert_eq!(context.dropped + context.events.len() as u64, context.count);
    }

    #[test]
    fn diagnostics_panic_hook_records_context() {
        const PROBE: &str = "PP_TEST_DIAGNOSTIC_PANIC";
        if std::env::var_os(PROBE).is_some() {
            let _guard = crate::logging::init().unwrap();
            register_secret("panic-secret-fixture");
            panic!("panic-evidence panic-secret-fixture");
        }
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "logging::diagnostics::tests::diagnostics_panic_hook_records_context",
            ])
            .env(PROBE, "1")
            .env("HOME", dir.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let path = std::fs::read_dir(dir.path().join(".pp/logs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "log"))
            .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains("panic-secret-fixture"));
        let record: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(record["fields"]["event"], "panic");
        assert!(
            record["fields"]["detail"]
                .as_str()
                .unwrap()
                .contains("panic-evidence")
        );
        assert!(
            record["fields"]["backtrace"]
                .as_str()
                .unwrap()
                .contains("diagnostics_panic_hook_records_context")
        );
    }
}
