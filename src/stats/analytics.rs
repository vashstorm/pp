use super::{COLUMNS, Range, RequestRow, Store, row};
use anyhow::Result;
use chrono::{Local, TimeZone, Timelike};
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Serialize)]
pub(super) struct Price {
    input: f64,
    cached_input: f64,
    output: f64,
}
#[derive(Default)]
pub(super) struct Pricing {
    pub rates: BTreeMap<String, Price>,
    pub error: Option<&'static str>,
}
impl Pricing {
    pub fn load(store: &Store) -> Self {
        match crate::models::load_models(&store.models_path) {
            Ok(providers) => {
                let mut rates = BTreeMap::new();
                for provider in providers {
                    for model in provider.models {
                        if let Some(cost) = model.cost {
                            rates.insert(
                                format!("{}/{}", provider.id, model.id),
                                Price {
                                    input: cost.input,
                                    cached_input: cost.cache_read,
                                    output: cost.output,
                                },
                            );
                        }
                    }
                }
                Self { rates, error: None }
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Self::default()
            }
            Err(_) => Self {
                error: Some(
                    "无法从 ~/.pp/agent/models.yml 读取价格：请检查模型配置及 cost.input、cost.cacheRead、cost.output 单价（USD／百万 Token）。",
                ),
                ..Self::default()
            },
        }
    }
    pub fn cost(&self, r: &RequestRow) -> Option<Cost> {
        let p = self.rates.get(&r.model_reference)?;
        let input = r.input_tokens?;
        let output = r.output_tokens?;
        let cached = r.cached_tokens?;
        if input < 0 || output < 0 || cached < 0 || cached > input {
            return None;
        }
        let cost = Cost {
            input: (input - cached) as f64 * (p.input / 1e6),
            cache_read: cached as f64 * (p.cached_input / 1e6),
            output: output as f64 * (p.output / 1e6),
        };
        cost.total().is_finite().then_some(cost)
    }
    pub fn request(&self, r: &RequestRow) -> Result<Value> {
        let mut value = serde_json::to_value(r)?;
        let cost = self.cost(r);
        value["cost"] = json!(cost.as_ref().map(Cost::total));
        value["cost_components"] = json!(cost);
        Ok(value)
    }
}
#[derive(Default, Serialize)]
pub(super) struct Cost {
    input: f64,
    cache_read: f64,
    output: f64,
}
impl Cost {
    pub(super) fn total(&self) -> f64 {
        self.input + self.cache_read + self.output
    }
}
#[derive(Default)]
pub(super) struct Aggregate {
    requests: u64,
    ok: u64,
    failed: u64,
    aborted: u64,
    input: Option<i64>,
    output: Option<i64>,
    cached: Option<i64>,
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
    components: Cost,
}
fn add(total: &mut Option<i64>, value: Option<i64>) {
    if let Some(v) = value {
        *total = Some(total.unwrap_or(0) + v);
    }
}
impl Aggregate {
    pub(super) fn push(&mut self, r: &RequestRow, cost: Option<&Cost>) {
        self.requests += 1;
        self.ok += u64::from(r.status == "ok");
        self.failed += u64::from(r.status == "failed");
        self.aborted += u64::from(r.status == "aborted");
        add(&mut self.input, r.input_tokens);
        add(&mut self.output, r.output_tokens);
        add(&mut self.cached, r.cached_tokens);
        add(
            &mut self.tokens,
            r.input_tokens.zip(r.output_tokens).map(|(i, o)| i + o),
        );
        add(
            &mut self.uncached,
            r.input_tokens
                .zip(r.cached_tokens)
                .filter(|(i, c)| c <= i)
                .map(|(i, c)| i - c),
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
            "usage_known":self.usage_known,"cache_known":self.cache_known,
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
    fn push(&mut self, r: &RequestRow, cost: Option<&Cost>, bucket: i64, day: i64, hour: u32) {
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
    pub(super) fn analytics(&self, range: Range, now: i64, pricing: &Pricing) -> Result<Value> {
        let mut statement = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp,id"))?;
        let rows = statement.query_map(params![range.since(now), now], row)?;
        let mut all = Group::default();
        let mut models = BTreeMap::<String, Group>::new();
        let mut providers = BTreeMap::<String, Group>::new();
        let mut names = BTreeMap::new();
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
            let cost = pricing.cost(&r);
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
                v["rates"] = json!(pricing.rates.get(key));
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
            json!({"summary":all.totals.value(),"buckets":series(&all.buckets),"daily":series(&all.daily),"hours":all.value()["hours"],"models":model_values,"providers":provider_values,"active_days":all.daily.len(),"pricing_error":pricing.error,"pricing_source":"~/.pp/agent/models.yml · cost · USD per million tokens · current configured rates","range":range.label(),"updated_at":now}),
        )
    }
}
