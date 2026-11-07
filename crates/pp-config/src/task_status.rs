//! 当前任务的运行时快照；与会话 journal、统计落盘和 UI 生命周期独立。
use crate::config::ModelCatalog;
use crate::models::ModelCost;
use pp_core::context::TokenUsage;
use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);
pub fn request_id() -> u64 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub generation: u64,
    pub title: Option<String>,
    pub cost: f64,
    /// 生成中请求的实时预估费用之和；只在对应请求落定前有效。
    pub live: f64,
    pub incomplete: bool,
}

#[derive(Clone, Default)]
pub struct View(Arc<Mutex<ViewState>>);
#[derive(Default)]
struct ViewState {
    generation: u64,
    current: Option<Ledger>,
}

#[derive(Clone)]
pub struct Ledger(Arc<Mutex<State>>);
struct State {
    snapshot: Snapshot,
    prices: BTreeMap<String, ModelCost>,
    settled: HashSet<u64>,
    /// 生成中请求的预估费用，按请求 id 保存；并发子 Agent 各自占一条，落定时移除。
    live: BTreeMap<u64, f64>,
}

impl View {
    pub fn begin(&self, catalog: &ModelCatalog) -> Ledger {
        let prices = catalog
            .choices()
            .iter()
            .filter_map(|model| {
                catalog
                    .cost(&model.reference)
                    .map(|price| (model.reference.clone(), price))
            })
            .collect();
        self.begin_prices(prices)
    }

    fn begin_prices(&self, prices: BTreeMap<String, ModelCost>) -> Ledger {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.generation += 1;
        let ledger = Ledger(Arc::new(Mutex::new(State {
            snapshot: Snapshot {
                generation: state.generation,
                ..Snapshot::default()
            },
            prices,
            settled: HashSet::new(),
            live: BTreeMap::new(),
        })));
        state.current = Some(ledger.clone());
        ledger
    }

    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).current = None;
    }

    pub fn current(&self) -> Option<Ledger> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .clone()
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        let ledger = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .clone()?;
        Some(ledger.snapshot())
    }
}

impl Ledger {
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .clone()
    }

    pub fn title(&self, title: String) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .title = Some(title);
    }

    /// 生成中的请求更新实时预估：输入与输出分别由调用方按请求体／已接收输出字节估算。
    /// 落定前统一按未命中缓存的输入价计算；未知价格不估算，也不标记 `incomplete`。
    pub fn progress(&self, id: u64, model: &str, input_tokens: u64, output_tokens: u64) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.settled.contains(&id) {
            return;
        }
        let estimate = state.prices.get(model).and_then(|price| {
            price.calculate(TokenUsage {
                input_tokens,
                output_tokens,
                cached_tokens: Some(0),
                cache_write_tokens: Some(0),
            })
        });
        match estimate.map(|cost| cost.total()) {
            Some(cost) => state.live.insert(id, cost),
            None => state.live.remove(&id),
        };
        refresh_live(&mut state);
    }

    pub fn settle(&self, id: u64, model: &str, usage: Option<TokenUsage>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.settled.insert(id) {
            return;
        }
        state.live.remove(&id);
        refresh_live(&mut state);
        let cost = usage.and_then(|usage| state.prices.get(model)?.calculate(usage));
        match cost.map(|cost| cost.total()) {
            Some(cost) if (state.snapshot.cost + cost).is_finite() => state.snapshot.cost += cost,
            _ => state.snapshot.incomplete = true,
        }
    }
}

/// 预估只是显示进度，溢出时整段丢弃，不能污染已落定金额。
fn refresh_live(state: &mut State) {
    let total: f64 = state.live.values().sum();
    state.snapshot.live = if total.is_finite() { total } else { 0.0 };
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prices() -> BTreeMap<String, ModelCost> {
        [
            (
                "p/main".into(),
                ModelCost {
                    input: 2.0,
                    output: 8.0,
                    cache_read: 0.2,
                    cache_write: Some(3.0),
                },
            ),
            (
                "p/fallback".into(),
                ModelCost {
                    input: 4.0,
                    output: 16.0,
                    cache_read: 0.4,
                    cache_write: None,
                },
            ),
        ]
        .into()
    }
    fn usage() -> TokenUsage {
        TokenUsage {
            input_tokens: 1000,
            output_tokens: 100,
            cached_tokens: Some(200),
            cache_write_tokens: Some(100),
        }
    }
    #[test]
    fn task_cost_counts_each_request_once_across_shared_nested_handles() {
        let view = View::default();
        let root = view.begin_prices(prices());
        let child = root.clone();
        let nested = child.clone();
        root.settle(1, "p/main", Some(usage()));
        child.settle(2, "p/main", Some(usage()));
        nested.settle(3, "p/main", Some(usage()));
        child.settle(2, "p/main", Some(usage()));
        assert!((view.snapshot().unwrap().cost - 0.00762).abs() < 1e-12);
        assert!(!view.snapshot().unwrap().incomplete);
    }
    #[test]
    fn task_cost_partial_unknown_and_fallback_use_actual_model_rates() {
        let view = View::default();
        let ledger = view.begin_prices(prices());
        ledger.settle(1, "p/main", Some(usage()));
        let no_write = TokenUsage {
            cache_write_tokens: Some(0),
            ..usage()
        };
        ledger.settle(2, "p/fallback", Some(no_write));
        ledger.settle(3, "unpriced/model", Some(usage()));
        ledger.settle(4, "p/main", None);
        ledger.settle(
            5,
            "p/main",
            Some(TokenUsage {
                cached_tokens: None,
                ..usage()
            }),
        );
        ledger.settle(6, "p/fallback", Some(usage()));
        ledger.settle(
            7,
            "p/main",
            Some(TokenUsage {
                cached_tokens: Some(1001),
                ..usage()
            }),
        );
        let snapshot = view.snapshot().unwrap();
        assert!((snapshot.cost - (0.00254 + 0.00488)).abs() < 1e-12);
        assert!(snapshot.incomplete);
    }
    #[test]
    fn task_status_generations_isolate_late_title_cost_and_clear() {
        let view = View::default();
        let old = view.begin_prices(prices());
        let current = view.begin_prices(prices());
        old.title("旧标题".into());
        old.settle(1, "p/main", Some(usage()));
        assert_eq!(view.snapshot().unwrap().generation, 2);
        assert_eq!(view.snapshot().unwrap().cost, 0.0);
        assert!(view.snapshot().unwrap().title.is_none());
        current.title("新标题".into());
        assert_eq!(view.snapshot().unwrap().title.as_deref(), Some("新标题"));
        view.clear();
        current.settle(2, "p/main", Some(usage()));
        assert!(view.snapshot().is_none());
    }
    #[test]
    fn task_cost_zero_and_tiny_prices_remain_known() {
        let view = View::default();
        let price = ModelCost {
            input: 0.0,
            cache_read: 0.0,
            output: 0.0,
            cache_write: None,
        };
        let ledger = view.begin_prices([("free/model".into(), price)].into());
        ledger.settle(
            1,
            "free/model",
            Some(TokenUsage {
                cache_write_tokens: Some(0),
                ..usage()
            }),
        );
        assert_eq!(ledger.snapshot().cost, 0.0);
        assert!(!ledger.snapshot().incomplete);
    }

    #[test]
    fn task_live_estimate_sums_in_flight_requests_and_settles_once() {
        let view = View::default();
        let ledger = view.begin_prices(prices());
        ledger.progress(1, "p/main", 1000, 100);
        assert!((ledger.snapshot().live - 0.0028).abs() < 1e-12);
        assert_eq!(ledger.snapshot().cost, 0.0, "估算不能进已落定金额");
        assert!(!ledger.snapshot().incomplete);
        // 并发的子 Agent 请求各自占一条估算，显示的是它们的和。
        ledger.progress(2, "p/main", 500, 0);
        assert!((ledger.snapshot().live - 0.0038).abs() < 1e-12);
        ledger.settle(1, "p/main", Some(usage()));
        let snapshot = ledger.snapshot();
        assert!((snapshot.cost - 0.00254).abs() < 1e-12);
        assert!((snapshot.live - 0.001).abs() < 1e-12, "落定后只留其他请求");
        // 迟到的进度不能把已落定的请求重新计入估算。
        ledger.progress(1, "p/main", 1000, 100);
        assert!((ledger.snapshot().live - 0.001).abs() < 1e-12);
        ledger.settle(2, "p/fallback", None);
        assert_eq!(ledger.snapshot().live, 0.0, "未知 usage 也清掉估算");
        assert!(ledger.snapshot().incomplete);
    }

    #[test]
    fn task_live_estimate_requires_price_without_zero_filling() {
        let view = View::default();
        let ledger = view.begin_prices(prices());
        ledger.progress(1, "unpriced/model", 1000, 100);
        assert_eq!(ledger.snapshot().live, 0.0);
        assert_eq!(ledger.snapshot().cost, 0.0);
        assert!(
            !ledger.snapshot().incomplete,
            "生成中缺价格只隐藏估算，不标记部分费用"
        );
        // 未知缓存量的模型在估算里按零缓存计价，不要求 cacheWrite 单价。
        ledger.progress(1, "p/fallback", 1000, 100);
        assert!((ledger.snapshot().live - 0.0056).abs() < 1e-12);
        // 换成无价格模型时立刻撤掉同一请求的估算，不保留旧值。
        ledger.progress(1, "unpriced/model", 1200, 200);
        assert_eq!(ledger.snapshot().live, 0.0);
    }
}
