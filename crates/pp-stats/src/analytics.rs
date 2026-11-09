use super::{COLUMNS, Range, RequestRow, Store, row};
use anyhow::Result;
use chrono::{Local, TimeZone, Timelike};
use pp_config::models::{ModelCost, ModelPricing};
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Serialize)]
struct Price {
    input: f64,
    cached_input: f64,
    cache_write: Option<f64>,
    output: f64,
}

impl From<ModelCost> for Price {
    fn from(cost: ModelCost) -> Self {
        Self {
            input: cost.input,
            cached_input: cost.cache_read,
            cache_write: cost.cache_write,
            output: cost.output,
        }
    }
}

#[derive(Default)]
pub(super) struct Pricing {
    rates: ModelPricing,
    pub error: Option<&'static str>,
}
impl Pricing {
    pub fn load(store: &Store) -> Self {
        match pp_config::models::load_models(&store.models_path) {
            Ok(providers) => Self {
                rates: ModelPricing::from_providers(&providers),
                error: None,
            },
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Self::default()
            }
            Err(_) => Self {
                error: Some(
                    "Unable to read pricing from ~/.pp/agent/models.yml: check the model configuration and the cost.input, cost.cacheRead, cost.cacheWrite, and cost.output rates (USD per million tokens).",
                ),
                ..Self::default()
            },
        }
    }

    pub(super) fn rate(&self, reference: &str, model: &str) -> Option<ModelCost> {
        self.error
            .is_none()
            .then(|| self.rates.rate(reference, model))
            .flatten()
    }

    pub fn cost(&self, r: &RequestRow) -> Option<Cost> {
        calculate(
            self.rate(&r.model_reference, &r.model)?,
            r.input_tokens,
            r.output_tokens,
            r.cached_tokens,
            r.cache_write_tokens,
        )
    }

    pub fn request(&self, r: &RequestRow) -> Result<Value> {
        let mut value = serde_json::to_value(r)?;
        let cost = self.cost(r);
        value["cost"] = json!(cost.as_ref().map(Cost::total));
        value["cost_components"] = json!(cost);
        Ok(value)
    }
}
pub(super) use pp_config::models::TokenCost as Cost;
pub(super) fn calculate(
    price: ModelCost,
    input: Option<i64>,
    output: Option<i64>,
    cached: Option<i64>,
    written: Option<i64>,
) -> Option<Cost> {
    price.calculate(pp_core::context::TokenUsage {
        input_tokens: input?.try_into().ok()?,
        output_tokens: output?.try_into().ok()?,
        cached_tokens: Some(cached?.try_into().ok()?),
        // 写入量未知时保留 None，由费率决定能否计价。
        cache_write_tokens: written.map(u64::try_from).transpose().ok()?,
    })
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
        let mut model_rates = BTreeMap::new();
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
            let rate = pricing.rate(&r.model_reference, &r.model);
            // 同一配置引用可能在历史上指向不同实际模型，仅显示一致的费率。
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
                v["rates"] = json!(model_rates[key].map(Price::from));
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
            json!({"summary":all.totals.value(),"buckets":series(&all.buckets),"daily":series(&all.daily),"hours":all.value()["hours"],"models":model_values,"providers":provider_values,"active_days":all.daily.len(),"pricing_error":pricing.error,"pricing_source":"~/.pp/agent/models.yml overrides built-in rates · USD per million tokens","range":range.label(),"updated_at":now}),
        )
    }
}
