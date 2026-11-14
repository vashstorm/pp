use super::{COLUMNS, CostSnapshot, Range, RateSnapshot, RequestRow, Store, row};
use anyhow::Result;
use chrono::{Local, TimeZone, Timelike};
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Aggregate {
    requests: u64,
    ok: u64,
    failed: u64,
    aborted: u64,
    input: Option<i64>,
    output: Option<i64>,
    cached: Option<i64>,
    cache_write: Option<i64>,
    cache_write_known: u64,
    uncached: Option<i64>,
    tokens: Option<i64>,
    usage_known: u64,
    cache_known: u64,
    priced: u64,
    duration: f64,
    ttft: f64,
    ttft_count: u64,
    speed: f64,
    speed_count: u64,
    first: Option<i64>,
    last: Option<i64>,
    components: CostSnapshot,
}
fn add(total: &mut Option<i64>, value: Option<i64>) {
    if let Some(v) = value {
        *total = Some(total.unwrap_or(0) + v);
    }
}
impl Aggregate {
    pub(super) fn push(&mut self, r: &RequestRow, cost: Option<&CostSnapshot>) {
        self.requests += 1;
        self.ok += u64::from(r.status == "ok");
        self.failed += u64::from(r.status == "failed");
        self.aborted += u64::from(r.status == "aborted");
        add(&mut self.input, r.input_tokens);
        add(&mut self.output, r.output_tokens);
        add(&mut self.cached, r.cached_tokens);
        add(&mut self.cache_write, r.cache_write_tokens);
        self.cache_write_known += u64::from(r.cache_write_tokens.is_some());
        add(
            &mut self.tokens,
            r.input_tokens.zip(r.output_tokens).map(|(i, o)| i + o),
        );
        add(
            &mut self.uncached,
            r.input_tokens
                .zip(r.cached_tokens)
                .zip(r.cache_write_tokens)
                .filter(|((i, c), w)| *c >= 0 && *w >= 0 && c <= i && *w <= i - c)
                .map(|((i, c), w)| i - c - w),
        );
        self.usage_known += u64::from(r.input_tokens.is_some() && r.output_tokens.is_some());
        self.cache_known += u64::from(r.cached_tokens.is_some());
        self.duration += r.duration_ms;
        if let Some(ttft) = r.ttft_ms {
            self.ttft += ttft;
            self.ttft_count += 1;
        }
        if let Some(output) = r.output_tokens.filter(|_| r.duration_ms > 0.0) {
            self.speed += output as f64 * 1000.0 / r.duration_ms;
            self.speed_count += 1;
        }
        self.first = Some(self.first.map_or(r.timestamp, |t| t.min(r.timestamp)));
        self.last = Some(self.last.map_or(r.timestamp, |t| t.max(r.timestamp)));
        if let Some(c) = cost {
            self.priced += 1;
            self.components.input += c.input;
            self.components.cache_read += c.cache_read;
            self.components.cache_write += c.cache_write;
            self.components.output += c.output;
        }
    }
    pub(super) fn value(&self) -> Value {
        let avg = |value, count| {
            if count > 0 {
                Some(value / count as f64)
            } else {
                None
            }
        };
        let tokens = |v: Option<i64>| if self.requests == 0 { Some(0) } else { v };
        json!({
            "requests":self.requests,"ok":self.ok,"failed":self.failed,"aborted":self.aborted,
            "input_tokens":tokens(self.input),"output_tokens":tokens(self.output),"cached_tokens":tokens(self.cached),"uncached_tokens":tokens(self.uncached),"tokens":tokens(self.tokens),
            "usage_known":self.usage_known,"cache_known":self.cache_known,"cache_write_known":self.cache_write_known,"cache_write_tokens":tokens(self.cache_write),
            "avg_duration_ms":avg(self.duration,self.requests),"avg_ttft_ms":avg(self.ttft,self.ttft_count),"tokens_per_second":avg(self.speed,self.speed_count),
            "first_seen":self.first,"last_seen":self.last,
            "priced":self.priced,"unpriced":self.requests-self.priced,
            "cost":if self.priced > 0 || self.requests == 0 { Some(self.components.total()) } else { None },
            "cost_components":if self.priced > 0 || self.requests == 0 { Some(&self.components) } else { None },
        })
    }
}
#[derive(Default)]
struct Group {
    totals: Aggregate,
    buckets: BTreeMap<i64, Aggregate>,
    daily: BTreeMap<i64, Aggregate>,
    models: BTreeMap<String, String>,
    hours: BTreeMap<u32, Aggregate>,
}
impl Group {
    fn value(&self) -> Value {
        let mut value = self.totals.value();
        value["buckets"] = series(&self.buckets);
        value["daily"] = series(&self.daily);
        value["models"] = json!(self.models.len());
        value["hours"] = json!(
            (0..24)
                .map(|hour| {
                    let mut v = self
                        .hours
                        .get(&hour)
                        .map(Aggregate::value)
                        .unwrap_or_else(|| Aggregate::default().value());
                    v["hour"] = json!(hour);
                    v
                })
                .collect::<Vec<_>>()
        );
        value
    }
    fn push(
        &mut self,
        r: &RequestRow,
        cost: Option<&CostSnapshot>,
        bucket: i64,
        day: i64,
        hour: u32,
    ) {
        self.totals.push(r, cost);
        self.buckets.entry(bucket).or_default().push(r, cost);
        self.daily.entry(day).or_default().push(r, cost);
        self.hours.entry(hour).or_default().push(r, cost);
        self.models
            .insert(r.model_reference.clone(), r.model.clone());
    }
}
fn series(buckets: &BTreeMap<i64, Aggregate>) -> Value {
    json!(
        buckets
            .iter()
            .map(|(timestamp, a)| {
                let mut v = a.value();
                v["timestamp"] = json!(timestamp);
                v
            })
            .collect::<Vec<_>>()
    )
}
impl Store {
    pub(super) fn analytics(&self, range: Range, now: i64) -> Result<Value> {
        let mut statement = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp,id"))?;
        let rows = statement.query_map(params![range.since(now), now], row)?;
        let mut all = Group::default();
        let mut models = BTreeMap::<String, Group>::new();
        let mut providers = BTreeMap::<String, Group>::new();
        let mut names = BTreeMap::new();
        let mut model_rates = BTreeMap::<String, Option<RateSnapshot>>::new();
        for r in rows {
            let r = r?;
            let local = Local
                .timestamp_millis_opt(r.timestamp)
                .single()
                .ok_or_else(|| anyhow::anyhow!("invalid request timestamp"))?;
            let day = r.timestamp.div_euclid(86_400_000) * 86_400_000;
            let bucket = match range {
                Range::Hour => r.timestamp.div_euclid(300_000) * 300_000,
                Range::Day => r.timestamp.div_euclid(3_600_000) * 3_600_000,
                _ => local
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .and_then(|d| d.and_local_timezone(Local).earliest())
                    .map(|d| d.timestamp_millis())
                    .ok_or_else(|| anyhow::anyhow!("invalid local date"))?,
            };
            let cost = r.cost();
            all.push(&r, cost.as_ref(), bucket, day, local.hour());
            models.entry(r.model_reference.clone()).or_default().push(
                &r,
                cost.as_ref(),
                bucket,
                day,
                local.hour(),
            );
            providers.entry(r.provider.clone()).or_default().push(
                &r,
                cost.as_ref(),
                bucket,
                day,
                local.hour(),
            );
            let rate = r.rates();
            // 同一配置引用可能在历史上指向不同实际模型或缺少费率快照，仅显示一致的费率。
            model_rates
                .entry(r.model_reference.clone())
                .and_modify(|previous| {
                    if *previous != rate {
                        *previous = None;
                    }
                })
                .or_insert(rate);
            names.insert(r.model_reference.clone(), (r.model, r.provider));
        }
        let model_values: Vec<Value> = models
            .iter()
            .map(|(key, g)| {
                let mut v = g.value();
                let (model, provider) = &names[key];
                v["key"] = json!(key);
                v["model"] = json!(model);
                v["provider"] = json!(provider);
                v["rates"] = json!(model_rates[key]);
                v
            })
            .collect();
        let provider_values: Vec<Value> = providers
            .iter()
            .map(|(key, g)| {
                let mut v = g.value();
                v["key"] = json!(key);
                v["provider"] = json!(key);
                v
            })
            .collect();
        Ok(
            json!({"summary":all.totals.value(),"buckets":series(&all.buckets),"daily":series(&all.daily),"hours":all.value()["hours"],"models":model_values,"providers":provider_values,"active_days":all.daily.len(),"range":range.label(),"updated_at":now}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::{CostSnapshot, Range, Rates, RequestRow, ensure_schema, server};
    use rusqlite::Connection;
    use std::fs;
    use std::path::PathBuf;

    fn record(
        key: &str,
        reference: &str,
        cost: Option<CostSnapshot>,
        rates: Option<RateSnapshot>,
    ) -> RequestRow {
        RequestRow {
            id: 0,
            request_key: key.to_owned(),
            timestamp: 1_000,
            model: "fixture-model".to_owned(),
            provider: "fixture".to_owned(),
            model_reference: reference.to_owned(),
            api: "openai".to_owned(),
            project: "project".to_owned(),
            cwd: "/tmp".to_owned(),
            purpose: "chat".to_owned(),
            session_file: None,
            entry_id: None,
            duration_ms: 1.0,
            ttft_ms: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
            cached_tokens: Some(0),
            cache_write_tokens: Some(0),
            status: "ok".to_owned(),
            stop_reason: None,
            error_message: None,
            cost,
            rates,
        }
    }

    /// 模型费率取范围内记录的快照：一致时展示四个费率字段，缺失或漂移即为 null。
    #[test]
    fn model_rates_show_recorded_snapshots_only_when_consistent() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&mut conn).unwrap();
        let mut store = Store {
            conn,
            models_path: PathBuf::from("/nonexistent/models.yml"),
        };
        let rate = RateSnapshot {
            input: 2.0,
            cached_input: 0.2,
            cache_write: None,
            output: 8.0,
        };
        let cost = CostSnapshot {
            input: 1.0,
            cache_read: 0.0,
            cache_write: 0.0,
            output: 2.0,
        };
        for (key, reference, rate) in [
            ("a", "fixture/primary", Some(rate.clone())),
            ("b", "fixture/primary", Some(rate.clone())),
            ("c", "fixture/missing", None),
            ("d", "fixture/drift", Some(rate.clone())),
            (
                "e",
                "fixture/drift",
                Some(RateSnapshot {
                    input: 3.0,
                    ..rate.clone()
                }),
            ),
        ] {
            let cost = (key != "c").then(|| cost.clone());
            store
                .insert(&record(key, reference, cost, rate), &Value::Null)
                .unwrap();
        }
        let value = store.analytics(Range::All, 10_000).unwrap();
        assert!(value.get("pricing_error").is_none());
        assert!(value.get("pricing_source").is_none());
        assert_eq!(value["summary"]["priced"], 4);
        assert_eq!(value["summary"]["unpriced"], 1);
        assert!((value["summary"]["cost"].as_f64().unwrap() - 12.0).abs() < 1e-12);
        assert_eq!(
            value["summary"]["cost_components"],
            json!({"input":4.0,"cache_read":0.0,"cache_write":0.0,"output":8.0})
        );
        let models = value["models"].as_array().unwrap();
        let rates =
            |key: &str| models.iter().find(|model| model["key"] == key).unwrap()["rates"].clone();
        assert_eq!(
            rates("fixture/primary"),
            json!({"input":2.0,"cached_input":0.2,"cache_write":null,"output":8.0})
        );
        assert!(rates("fixture/missing").is_null());
        assert!(rates("fixture/drift").is_null());
    }

    #[test]
    fn stats_analytics_groups_all_rows_and_prices_cached_input_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        fs::write(
            dir.path().join("models.yml"),
            r#"# 价格与普通模型配置共用
providers:
  fixture:
    api_key: secret-marker-fixture-key
    models:
      primary:
        model: fixture-model
        cost: {input: 2, cacheRead: 0.2, output: 8}
      unpriced:
        model: fixture-model
  other:
    api_key: secret-marker-other-key
    models:
      primary:
        model: fixture-model
        cost: {input: 0, cacheRead: 0, output: 0}
"#,
        )
        .unwrap();
        let now = 1_800_000_000_000;
        let rates = Rates::load(&store.models_path);
        for n in 0..2005 {
            let mut record = fixture(&format!("priced-{n}"), now);
            snapshot_for(&rates, &mut record);
            store.insert(&record, &Value::Null).unwrap();
        }
        let mut other = fixture("other", now - 86_400_000);
        other.provider = "other".into();
        other.model_reference = "other/primary".into();
        other.status = "failed".into();
        snapshot_for(&rates, &mut other);
        store.insert(&other, &Value::Null).unwrap();
        let mut unknown = fixture("unknown", now);
        unknown.cached_tokens = None;
        unknown.ttft_ms = None;
        unknown.status = "aborted".into();
        snapshot_for(&rates, &mut unknown);
        assert!(unknown.cost.is_none());
        assert!(unknown.rates.is_some());
        store.insert(&unknown, &Value::Null).unwrap();
        store
            .insert(&fixture("outside", now + 1), &Value::Null)
            .unwrap();
        // 配置引用了没有价格的 model 时既无费用也无费率快照。
        let (alias_cost, alias_rate) = rates.snapshot(
            "fixture/unpriced",
            "fixture-model",
            Some(100),
            Some(20),
            Some(80),
            Some(0),
        );
        assert!(alias_cost.is_none());
        assert!(alias_rate.is_none());
        let result = store.analytics(Range::Day, now).unwrap();
        let s = &result["summary"];
        assert_eq!(s["requests"], 2007);
        assert_eq!(s["priced"], 2006);
        assert_eq!(s["unpriced"], 1);
        assert_eq!(s["failed"], 1);
        assert_eq!(s["aborted"], 1);
        assert_eq!(result["models"].as_array().unwrap().len(), 2);
        assert_eq!(result["providers"].as_array().unwrap().len(), 2);
        assert_eq!(result["active_days"], 2);
        let expected = 2005.0 * (20.0 * 2.0 + 80.0 * 0.2 + 20.0 * 8.0) / 1e6;
        assert!((s["cost"].as_f64().unwrap() - expected).abs() < 1e-10);
        for field in ["models", "providers", "buckets", "daily", "hours"] {
            let rows = result[field].as_array().unwrap();
            assert_eq!(
                rows.iter()
                    .map(|r| r["requests"].as_u64().unwrap())
                    .sum::<u64>(),
                2007
            );
            assert!(
                (rows.iter().filter_map(|r| r["cost"].as_f64()).sum::<f64>() - expected).abs()
                    < 1e-10
            );
        }
        let detail = store.detail(1).unwrap().unwrap();
        assert!((detail["request"]["cost"].as_f64().unwrap() - expected / 2005.0).abs() < 1e-10);
        assert_eq!(result["providers"][0]["models"], 1);
        assert_eq!(result["summary"]["avg_ttft_ms"], 500.0);
        assert_eq!(result["models"][0]["buckets"][0]["tokens_per_second"], 10.0);
    }
    #[test]
    fn stats_analytics_api_validates_ranges_and_omits_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        store
            .insert(
                &fixture("one", chrono::Utc::now().timestamp_millis()),
                &json!({"text":"private-output"}),
            )
            .unwrap();
        for endpoint in ["models", "providers", "costs", "projects", "tools"] {
            for range in ["1h", "24h", "7d", "30d", "90d", "all"] {
                let (code, _, body) =
                    server::dispatch(&store, &format!("/api/stats/{endpoint}?range={range}"), &[]);
                assert_eq!(code, 200);
                assert!(!body.contains("private-output"));
                if endpoint != "projects" {
                    assert!(!body.contains("session_file"));
                }
                let value: Value = serde_json::from_str(&body).unwrap();
                if endpoint == "tools" {
                    assert_eq!(value["summary"]["calls"], 0);
                    assert_eq!(value["summary"]["cost"], 0.0);
                } else {
                    assert_eq!(value["summary"]["requests"], 1);
                    assert_eq!(value["summary"]["unpriced"], 1);
                    assert!(value["summary"]["cost"].is_null());
                }
            }
        }
    }
}
