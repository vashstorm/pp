use pp_config::models::{ModelCost, ModelPricing};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 一次请求在采集时记录的费用分量（USD），字段与 `pp_config::models::TokenCost` 一致。
///
/// `requests.cost_json` 为 NULL 表示该请求未定价（旧记录或缺价格／usage）。
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

/// 采集时匹配到的费率（USD／百万 token）；`cache_write` 缺失表示该费率没有独立写入价。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RateSnapshot {
    pub input: f64,
    pub cached_input: f64,
    pub cache_write: Option<f64>,
    pub output: f64,
}

/// 采集时的计价依据：请求结束时读一次 `models.yml` 与内置表，之后只写快照。
///
/// `pricing` 为 `None` 表示模型配置无法解析：与旧行为一致，此时既不计价也不记录费率。
pub(crate) struct Rates {
    pricing: Option<ModelPricing>,
}

impl Rates {
    pub fn load(models_path: &Path) -> Self {
        let pricing = match pp_config::models::load_models(models_path) {
            Ok(providers) => Some(ModelPricing::from_providers(&providers)),
            // 没有 models.yml 时只保留内置价格表。
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

    /// 返回 `(费用, 费率)`：命中费率而 usage 不全时只记录费率，未命中费率时两者皆空。
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

/// 与 `pp_config::models::ModelCost::calculate` 共享严格规则：未知缓存读不补零。
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
        // 写入量未知时保留 None，由费率决定能否计价。
        cache_write_tokens: written.map(u64::try_from).transpose().ok()?,
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
    fn stats_cache_write_column_restore_does_not_invent_history() {
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
        // 会话关联只写 session_file，entry_id 不再由 link 写入。
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

    #[test]
    fn stats_cache_write_cost_and_aggregates_preserve_unknown_zero_and_invalid() {
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
        // 费率没有写入价时，写入量未知不影响总价，不能整条请求记为未定价。
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

    #[test]
    fn stats_rates_preserve_unknown_and_reject_invalid_config() {
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
        // 没有 models.yml 时只保留内置价格表。
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

        // 配置无法解析时完全不计价，连内置价格表也不用。
        for invalid in [
            models_with_cost(json!({"input":-1,"cacheRead":0,"output":1})),
            models_with_cost(json!({"input":1,"output":1})),
            models_with_cost(json!({"input":1,"cacheRead":0,"output":1,"typo":"secret-marker"})),
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
            models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8})),
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
        // 缓存读超过输入、缓存读未知、输出未知都不能按零计价。
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
