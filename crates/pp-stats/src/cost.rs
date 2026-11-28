use pp_config::models::{ModelCost, ModelPricing};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Cost components recorded for a request at collection time (USD); fields match
/// `pp_config::models::TokenCost`.
///
/// A NULL `requests.cost_json` means the request is unpriced (old record or missing price/usage).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CostSnapshot {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl CostSnapshot {
    pub fn total(&self) -> f64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

/// Rate matched at collection time (USD per million tokens); a missing `cache_write` means the
/// rate has no separate cache-write price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RateSnapshot {
    pub input: f64,
    pub cached_input: f64,
    pub cache_write: Option<f64>,
    pub output: f64,
}

/// Pricing basis at collection time: `models.yml` and the built-in table are read once when the
/// request finishes, and only snapshots are written afterwards.
///
/// `pricing` being `None` means the model config cannot be parsed: matching the previous behavior,
/// nothing is priced and no rate is recorded.
pub(crate) struct Rates {
    pricing: Option<ModelPricing>,
}

impl Rates {
    pub fn load(models_path: &Path) -> Self {
        let pricing = match pp_config::models::load_models(models_path) {
            Ok(providers) => Some(ModelPricing::from_providers(&providers)),
            // Without models.yml only the built-in price table remains.
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Some(ModelPricing::default())
            }
            Err(_) => None,
        };
        Self { pricing }
    }

    /// Returns `(cost, rate)`: when a rate matches but usage is incomplete only the rate is
    /// recorded; when no rate matches both are empty.
    pub fn snapshot(
        &self,
        reference: &str,
        model: &str,
        input: Option<i64>,
        output: Option<i64>,
        cached: Option<i64>,
        written: Option<i64>,
    ) -> (Option<CostSnapshot>, Option<RateSnapshot>) {
        let Some(price) = self
            .pricing
            .as_ref()
            .and_then(|pricing| pricing.rate(reference, model))
        else {
            return (None, None);
        };
        let rate = RateSnapshot {
            input: price.input,
            cached_input: price.cache_read,
            cache_write: price.cache_write,
            output: price.output,
        };
        (calculate(price, input, output, cached, written), Some(rate))
    }
}

/// Shares the strict rules of `pp_config::models::ModelCost::calculate`: unknown cache reads are not
/// zero-filled.
fn calculate(
    price: ModelCost,
    input: Option<i64>,
    output: Option<i64>,
    cached: Option<i64>,
    written: Option<i64>,
) -> Option<CostSnapshot> {
    let usage = pp_core::context::TokenUsage {
        input_tokens: input?.try_into().ok()?,
        output_tokens: output?.try_into().ok()?,
        cached_tokens: Some(cached?.try_into().ok()?),
        // Keep None when the write amount is unknown; the rate decides whether pricing is possible.
        cache_write_tokens: written.map(u64::try_from).transpose().ok()?,
        reasoning_tokens: None,
    };
    price.calculate(usage).map(|cost| CostSnapshot {
        input: cost.input,
        cache_read: cost.cache_read,
        cache_write: cost.cache_write,
        output: cost.output,
    })
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::{Range, RequestSnapshot};
    use serde_json::{Value, json};
    use std::fs;

    #[test]
    fn stats_cache_write_restore_costs_and_invalid_rates() {
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("stats.db");
            let mut store = open_store(&path).unwrap();
            let mut failed = fixture("old", 1000);
            failed.status = "failed".into();
            failed.error_message = Some("retained failure".into());
            let snapshot = RequestSnapshot {
                request: json!({"id":"old-entry","type":"message","message":{"role":"assistant","content":"retained snapshot"}}),
                skills: Vec::new(),
            };
            store
                .insert_with_context(&failed, &Value::Null, Some(&snapshot))
                .unwrap();
            store
                .link("old", Path::new("/missing/session.jsonl"))
                .unwrap();
            store
            .conn
            .execute_batch(
                "DROP INDEX requests_failed_time; ALTER TABLE requests DROP COLUMN cache_write_tokens;",
            )
            .unwrap();
            drop(store);
            let mut store = open_store(&path).unwrap();
            let old = store.detail(1).unwrap().unwrap();
            assert_eq!(old["request"]["cached_tokens"], 80);
            assert!(old["request"]["cache_write_tokens"].is_null());
            assert!(old.get("output_message").is_none());
            assert!(old.get("session_entry").is_none());
            assert_eq!(
                old["request_context"],
                serde_json::to_value(&snapshot).unwrap()
            );
            assert_eq!(old["request"]["status"], "failed");
            // Session linking only writes session_file; link no longer writes entry_id.
            assert!(old["request"]["entry_id"].is_null());
            assert_eq!(old["request"]["session_file"], "/missing/session.jsonl");
            let errors = store.errors(Range::All, 2000, 50).unwrap();
            assert_eq!(errors["requests"].as_array().unwrap().len(), 1);
            assert_eq!(errors["requests"][0]["error_message"], "retained failure");
            let mut new = fixture("new", 1001);
            new.cache_write_tokens = Some(10);
            store.insert(&new, &Value::Null).unwrap();
            drop(store);
            let store = open_store(&path).unwrap();
            assert_eq!(
                store.latest(0, 2000, 500).unwrap()[0].cache_write_tokens,
                Some(10)
            );
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let mut store = open_store(&dir.path().join("stats.db")).unwrap();
            let path = dir.path().join("models.yml");
            fs::write(
                &path,
                models_with_cost(json!({"input":2,"cacheRead":0.2,"cacheWrite":2.5,"output":8})),
            )
            .unwrap();
            let rates = Rates::load(&path);
            let mut record = fixture("written", 1000);
            record.cache_write_tokens = Some(10);
            let (cost, rate) = rates.snapshot(
                "fixture/primary",
                "fixture-model",
                Some(100),
                Some(20),
                Some(80),
                Some(10),
            );
            let cost = cost.unwrap();
            assert_eq!(cost.cache_write, 0.000025);
            assert!((cost.total() - 0.000221).abs() < 1e-10);
            assert_eq!(rate.as_ref().unwrap().cache_write, Some(2.5));
            record.cost = Some(cost);
            record.rates = rate;
            store.insert(&record, &Value::Null).unwrap();
            record.request_key = "zero".into();
            record.cache_write_tokens = Some(0);
            snapshot_for(&rates, &mut record);
            store.insert(&record, &Value::Null).unwrap();
            record.request_key = "unknown".into();
            record.cache_write_tokens = None;
            snapshot_for(&rates, &mut record);
            assert!(record.cost.is_none());
            assert!(record.rates.is_some());
            store.insert(&record, &Value::Null).unwrap();
            for written in [-1, 21, i64::MAX] {
                let (cost, rate) = rates.snapshot(
                    "fixture/primary",
                    "fixture-model",
                    Some(100),
                    Some(20),
                    Some(80),
                    Some(written),
                );
                assert!(cost.is_none(), "{written}");
                assert!(rate.is_some(), "{written}");
            }
            let result = store.analytics(Range::All, 2000).unwrap();
            for group in [
                result["summary"].clone(),
                result["models"][0].clone(),
                result["providers"][0].clone(),
                result["buckets"][0].clone(),
                result["daily"][0].clone(),
            ] {
                assert_eq!(group["cache_write_tokens"], 10);
                assert_eq!(group["cache_write_known"], 2);
                assert_eq!(group["uncached_tokens"], 30);
                assert_eq!(group["tokens"], 360);
                assert_eq!(group["priced"], 2);
                assert_eq!(group["unpriced"], 1);
                assert_eq!(group["cost_components"]["cache_write"], 0.000025);
                assert!((group["cost"].as_f64().unwrap() - 0.000437).abs() < 1e-10);
            }
            fs::write(
                &path,
                models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8})),
            )
            .unwrap();
            let rates = Rates::load(&path);
            let snapshot = |written: Option<i64>| {
                rates.snapshot(
                    "fixture/primary",
                    "fixture-model",
                    Some(100),
                    Some(20),
                    Some(80),
                    written,
                )
            };
            assert!(snapshot(Some(10)).0.is_none());
            assert!(snapshot(Some(0)).0.is_some());
            // When the rate has no cache-write price, an unknown write amount does not affect the
            // total, so the whole request must not be recorded as unpriced.
            assert!((snapshot(None).0.unwrap().total() - 0.000216).abs() < 1e-10);
            fs::write(
                &path,
                models_with_cost(json!({"input":0,"cacheRead":0,"cacheWrite":0,"output":0})),
            )
            .unwrap();
            let rates = Rates::load(&path);
            let (cost, _) = rates.snapshot(
                "fixture/primary",
                "fixture-model",
                Some(100),
                Some(20),
                Some(80),
                Some(10),
            );
            record.cost = cost;
            assert_eq!(record.value().unwrap()["cost"], 0.0);
            let empty = store.analytics(Range::Hour, 10_000_000).unwrap();
            assert_eq!(empty["summary"]["cache_write_tokens"], 0);
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let store = open_store(&dir.path().join("stats.db")).unwrap();
            let path = dir.path().join("models.yml");
            let unpriced = |rates: &Rates| {
                rates.snapshot(
                    "fixture/primary",
                    "fixture-model",
                    Some(100),
                    Some(20),
                    Some(80),
                    Some(0),
                )
            };
            // Without models.yml only the built-in price table remains.
            let rates = Rates::load(&path);
            assert!(unpriced(&rates).0.is_none());
            assert!(unpriced(&rates).1.is_none());
            let (cost, rate) = rates.snapshot(
                "openai/gpt-5.5",
                "openai/gpt-5.5",
                Some(1_000_000),
                Some(0),
                Some(0),
                Some(0),
            );
            assert_eq!(rate.unwrap().input, 5.0);
            assert!((cost.unwrap().total() - 5.0).abs() < 1e-12);

            // When the config cannot be parsed nothing is priced, not even with the built-in table.
            for invalid in [
                models_with_cost(json!({"input":-1,"cacheRead":0,"output":1})),
                models_with_cost(json!({"input":1,"output":1})),
                "invalid secret-marker".into(),
            ] {
                fs::write(&path, invalid).unwrap();
                let rates = Rates::load(&path);
                assert_eq!(unpriced(&rates), (None, None));
                assert_eq!(
                    rates.snapshot(
                        "openai/gpt-5.5",
                        "openai/gpt-5.5",
                        Some(1_000_000),
                        Some(0),
                        Some(0),
                        Some(0),
                    ),
                    (None, None)
                );
                assert!(store.overview(Range::All, 2000).is_ok());
            }

            fs::write(
                &path,
                models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8,"extra":"unused"})),
            )
            .unwrap();
            let rates = Rates::load(&path);
            let (cost, rate) = unpriced(&rates);
            assert!((cost.unwrap().total() - 0.000216).abs() < 1e-10);
            assert_eq!(rate.unwrap().cached_input, 0.2);

            fs::write(
                &path,
                models_with_cost(json!({"input":0,"cacheRead":0,"output":0})),
            )
            .unwrap();
            let rates = Rates::load(&path);
            assert_eq!(unpriced(&rates).0.unwrap().total(), 0.0);
            // A cache read larger than the input, an unknown cache read, or an unknown output must
            // not be priced as zero.
            for (input, output, cached) in [
                (Some(100), Some(20), Some(101)),
                (Some(100), Some(20), None),
                (Some(100), None, Some(80)),
            ] {
                let (cost, rate) = rates.snapshot(
                    "fixture/primary",
                    "fixture-model",
                    input,
                    output,
                    cached,
                    Some(0),
                );
                assert!(cost.is_none());
                assert!(rate.is_some());
            }
            let empty = store.analytics(Range::All, 2000).unwrap();
            assert_eq!(empty["summary"]["cost"], 0.0);
            assert_eq!(empty["summary"]["unpriced"], 0);
            assert_eq!(empty["summary"]["tokens"], 0);
        }
    }
}
