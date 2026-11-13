use anyhow::Result;

#[derive(Clone, Copy)]
pub(crate) enum Range {
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    All,
}
impl Range {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "1h" => Self::Hour,
            "24h" => Self::Day,
            "7d" => Self::Week,
            "30d" => Self::Month,
            "90d" => Self::Quarter,
            "all" => Self::All,
            _ => anyhow::bail!("invalid time range"),
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Quarter => "90d",
            Self::All => "all",
        }
    }
    pub fn since(self, now: i64) -> i64 {
        let hours = match self {
            Self::Hour => 1,
            Self::Day => 24,
            Self::Week => 168,
            Self::Month => 720,
            Self::Quarter => 2160,
            Self::All => return 0,
        };
        now.saturating_sub(hours * 3600000)
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use serde_json::Value;

    #[test]
    fn stats_ranges_include_boundaries_and_preserve_unknown_usage() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = 10_000_000;
        store
            .insert(&fixture("boundary", now - 3600000), &Value::Null)
            .unwrap();
        store
            .insert(&fixture("outside", now - 3600001), &Value::Null)
            .unwrap();
        let mut missing = fixture("missing", now);
        missing.input_tokens = None;
        missing.output_tokens = None;
        missing.cached_tokens = None;
        missing.cache_write_tokens = None;
        missing.ttft_ms = None;
        missing.status = "aborted".into();
        store.insert(&missing, &Value::Null).unwrap();
        store
            .insert(&fixture("future", now + 1), &Value::Null)
            .unwrap();
        let summary = store.overview(Range::Hour, now).unwrap();
        let s = &summary["summary"];
        assert_eq!(s["requests"], 2);
        assert_eq!(s["aborted"], 1);
        assert_eq!(s["input_tokens"], 100);
        assert_eq!(s["uncached_tokens"], 20);
        assert_eq!(s["output_tokens"], 20);
        assert_eq!(s["usage_known"], 1);
        assert_eq!(s["avg_ttft_ms"], 500.0);
        assert_eq!(
            store.overview(Range::All, now).unwrap()["summary"]["requests"],
            3
        );
        assert!(store.detail(999).unwrap().is_none());
        for label in ["1h", "24h", "7d", "30d", "90d", "all"] {
            assert!(Range::parse(label).is_ok());
        }
        assert!(Range::parse("1d").is_err());
    }
}
