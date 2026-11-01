//! 当前任务的运行时快照；与会话 journal、统计落盘和 UI 生命周期独立。
use crate::{config::ModelCatalog, context::TokenUsage, models::ModelCost};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);
pub(crate) fn request_id() -> u64 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub generation: u64,
    pub title: Option<String>,
    pub cost: f64,
    pub incomplete: bool,
}

#[derive(Clone, Default)]
pub(crate) struct View(Arc<Mutex<ViewState>>);
#[derive(Default)]
struct ViewState {
    generation: u64,
    current: Option<Ledger>,
}

#[derive(Clone)]
pub(crate) struct Ledger(Arc<Mutex<State>>);
struct State {
    snapshot: Snapshot,
    prices: BTreeMap<String, ModelCost>,
    settled: HashSet<u64>,
}

impl View {
    pub(crate) fn begin(&self, catalog: &ModelCatalog) -> Ledger {
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
        })));
        state.current = Some(ledger.clone());
        ledger
    }

    pub(crate) fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).current = None;
    }

    pub(crate) fn current(&self) -> Option<Ledger> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .clone()
    }

    pub(crate) fn snapshot(&self) -> Option<Snapshot> {
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

    pub(crate) fn title(&self, title: String) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .title = Some(title);
    }

    pub(crate) fn settle(&self, id: u64, model: &str, usage: Option<TokenUsage>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.settled.insert(id) {
            return;
        }
        let cost = usage.and_then(|usage| state.prices.get(model)?.calculate(usage));
        match cost.map(|cost| cost.total()) {
            Some(cost) if (state.snapshot.cost + cost).is_finite() => state.snapshot.cost += cost,
            _ => state.snapshot.incomplete = true,
        }
    }
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
}
