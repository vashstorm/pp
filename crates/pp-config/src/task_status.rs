//! Runtime snapshot of session cost and the current task title.
use crate::config::ModelCatalog;
use crate::models::{ModelCost, ModelPricing};
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
    pub history: Option<HistoricalCost>,
    /// Sum of live estimates for in-flight requests; valid only until those requests settle.
    pub live: f64,
    pub incomplete: bool,
}

/// USD subtotal recomputed from the existing statistics records. The amount is unknown when no records are priceable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HistoricalCost {
    pub usd: Option<f64>,
    pub incomplete: bool,
}

#[derive(Clone, Default)]
pub struct View(Arc<Mutex<Option<Ledger>>>);

#[derive(Clone)]
pub struct Ledger {
    state: Arc<Mutex<State>>,
    prices: ModelPricing,
    title_generation: u64,
}

#[derive(Default)]
struct State {
    snapshot: Snapshot,
    settled: HashSet<u64>,
    /// Live estimates keyed by request ID; removed once the request settles.
    live: BTreeMap<u64, f64>,
}

impl View {
    pub fn begin(&self, catalog: &ModelCatalog) -> Ledger {
        self.with_prices(catalog.pricing().clone(), true)
    }

    pub fn accounting(&self, catalog: &ModelCatalog) -> Ledger {
        self.with_prices(catalog.pricing().clone(), false)
    }

    fn with_prices(&self, prices: ModelPricing, new_title: bool) -> Ledger {
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let ledger = current.get_or_insert_with(|| Ledger::new(prices.clone(), None));
        ledger.prices = prices;
        if new_title {
            ledger.title_generation = ledger.clear_title();
        }
        ledger.clone()
    }

    pub fn restore(&self, catalog: &ModelCatalog, history: HistoricalCost) -> Ledger {
        let ledger = Ledger::new(catalog.pricing().clone(), Some(history));
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(ledger.clone());
        ledger
    }

    pub fn clear(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn clear_title(&self) {
        if let Some(ledger) = self.current() {
            ledger.clear_title();
        }
    }

    fn current(&self) -> Option<Ledger> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        Some(self.current()?.snapshot())
    }
}

impl Ledger {
    fn new(prices: ModelPricing, history: Option<HistoricalCost>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                snapshot: Snapshot {
                    history,
                    ..Snapshot::default()
                },
                ..State::default()
            })),
            prices,
            title_generation: 0,
        }
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .clone()
    }

    fn clear_title(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.snapshot.title = None;
        state.snapshot.generation += 1;
        state.snapshot.generation
    }

    pub fn title(&self, title: String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.snapshot.generation == self.title_generation {
            state.snapshot.title = Some(title);
        }
    }

    pub fn rate(&self, reference: &str, model: &str) -> Option<ModelCost> {
        self.prices.rate(reference, model)
    }

    /// In-flight requests publish live estimates: callers estimate input and output from the request body and received output bytes respectively.
    /// Until settlement everything is priced as cache-missing input; unknown prices are not estimated and do not mark `incomplete`.
    pub fn progress(
        &self,
        id: u64,
        price: Option<ModelCost>,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.settled.contains(&id) {
            return;
        }
        let estimate = price.and_then(|price| {
            price.calculate(TokenUsage {
                input_tokens,
                output_tokens,
                cached_tokens: Some(0),
                cache_write_tokens: Some(0),
                reasoning_tokens: None,
            })
        });
        match estimate.map(|cost| cost.total()) {
            Some(cost) => state.live.insert(id, cost),
            None => state.live.remove(&id),
        };
        refresh_live(&mut state);
    }

    pub fn settle(&self, id: u64, price: Option<ModelCost>, usage: Option<TokenUsage>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.settled.insert(id) {
            return;
        }
        state.live.remove(&id);
        refresh_live(&mut state);
        let cost = usage.and_then(|usage| price?.calculate(usage));
        match cost.map(|cost| cost.total()) {
            Some(cost) if (state.snapshot.cost + cost).is_finite() => state.snapshot.cost += cost,
            _ => state.snapshot.incomplete = true,
        }
    }
}

/// Estimates only show progress; on overflow the whole total is dropped so settled amounts stay clean.
fn refresh_live(state: &mut State) {
    let total: f64 = state.live.values().sum();
    state.snapshot.live = if total.is_finite() { total } else { 0.0 };
}

#[cfg(test)]
mod tests {
    use super::*;
    impl View {
        fn begin_prices(&self, prices: BTreeMap<String, ModelCost>) -> Ledger {
            self.with_prices(
                ModelPricing {
                    overrides: Arc::new(prices),
                },
                true,
            )
        }
    }

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
            reasoning_tokens: None,
        }
    }
    #[test]
    fn task_cost_counts_each_request_once_across_shared_nested_handles() {
        let view = View::default();
        let root = view.begin_prices(prices());
        let child = root.clone();
        let nested = child.clone();
        root.settle(1, root.rate("p/main", ""), Some(usage()));
        child.settle(2, child.rate("p/main", ""), Some(usage()));
        nested.settle(3, nested.rate("p/main", ""), Some(usage()));
        child.settle(2, child.rate("p/main", ""), Some(usage()));
        assert!((view.snapshot().unwrap().cost - 0.00762).abs() < 1e-12);
        assert!(!view.snapshot().unwrap().incomplete);
    }
    #[test]
    fn task_cost_partial_unknown_and_fallback_use_actual_model_rates() {
        let view = View::default();
        let ledger = view.begin_prices(prices());
        ledger.settle(1, ledger.rate("p/main", ""), Some(usage()));
        let no_write = TokenUsage {
            cache_write_tokens: Some(0),
            ..usage()
        };
        ledger.settle(2, ledger.rate("p/fallback", ""), Some(no_write));
        ledger.settle(3, ledger.rate("unpriced/model", ""), Some(usage()));
        ledger.settle(4, ledger.rate("p/main", ""), None);
        ledger.settle(
            5,
            ledger.rate("p/main", ""),
            Some(TokenUsage {
                cached_tokens: None,
                ..usage()
            }),
        );
        ledger.settle(6, ledger.rate("p/fallback", ""), Some(usage()));
        ledger.settle(
            7,
            ledger.rate("p/main", ""),
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
    fn task_titles_are_isolated_and_session_cost_survives_task_changes() {
        let view = View::default();
        let old = view.begin_prices(prices());
        let current = view.begin_prices(prices());
        old.title("Old title".into());
        old.settle(1, old.rate("p/main", ""), Some(usage()));
        assert_eq!(view.snapshot().unwrap().generation, 2);
        assert!((view.snapshot().unwrap().cost - 0.00254).abs() < 1e-12);
        assert!(view.snapshot().unwrap().title.is_none());
        current.title("New title".into());
        assert_eq!(view.snapshot().unwrap().title.as_deref(), Some("New title"));
        view.clear();
        current.settle(2, current.rate("p/main", ""), Some(usage()));
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
            ledger.rate("free/model", ""),
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
        ledger.progress(1, ledger.rate("p/main", ""), 1000, 100);
        assert!((ledger.snapshot().live - 0.0028).abs() < 1e-12);
        assert_eq!(
            ledger.snapshot().cost,
            0.0,
            "estimates must not enter settled cost"
        );
        assert!(!ledger.snapshot().incomplete);
        // Concurrent subagent requests each hold one estimate; the display is their sum.
        ledger.progress(2, ledger.rate("p/main", ""), 500, 0);
        assert!((ledger.snapshot().live - 0.0038).abs() < 1e-12);
        ledger.settle(1, ledger.rate("p/main", ""), Some(usage()));
        let snapshot = ledger.snapshot();
        assert!((snapshot.cost - 0.00254).abs() < 1e-12);
        assert!(
            (snapshot.live - 0.001).abs() < 1e-12,
            "only other requests remain after settlement"
        );
        // Late progress must not count a settled request in the estimates again.
        ledger.progress(1, ledger.rate("p/main", ""), 1000, 100);
        assert!((ledger.snapshot().live - 0.001).abs() < 1e-12);
        ledger.settle(2, ledger.rate("p/fallback", ""), None);
        assert_eq!(
            ledger.snapshot().live,
            0.0,
            "unknown usage also clears the estimate"
        );
        assert!(ledger.snapshot().incomplete);
    }

    #[test]
    fn task_live_estimate_requires_price_without_zero_filling() {
        let view = View::default();
        let ledger = view.begin_prices(prices());
        ledger.progress(1, ledger.rate("unpriced/model", ""), 1000, 100);
        assert_eq!(ledger.snapshot().live, 0.0);
        assert_eq!(ledger.snapshot().cost, 0.0);
        assert!(
            !ledger.snapshot().incomplete,
            "a missing price during generation only hides the estimate and never marks partial cost"
        );
        // Models with unknown cache volume are priced as zero-cache in estimates and do not require a cacheWrite rate.
        ledger.progress(1, ledger.rate("p/fallback", ""), 1000, 100);
        assert!((ledger.snapshot().live - 0.0056).abs() < 1e-12);
        // Switching to an unpriced model withdraws the estimate for the same request immediately instead of keeping the old value.
        ledger.progress(1, ledger.rate("unpriced/model", ""), 1200, 200);
        assert_eq!(ledger.snapshot().live, 0.0);
    }
}
