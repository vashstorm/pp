use super::{CostSnapshot, RateSnapshot};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RequestRow {
    pub id: i64,
    pub request_key: String,
    pub timestamp: i64,
    pub model: String,
    pub provider: String,
    pub model_reference: String,
    pub api: String,
    pub project: String,
    pub cwd: String,
    pub purpose: String,
    pub session_file: Option<String>,
    pub entry_id: Option<String>,
    pub duration_ms: f64,
    pub ttft_ms: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub status: String,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    /// Cost snapshot recorded at collection time; legacy rows without one stay unpriced.
    #[serde(skip)]
    pub cost: Option<CostSnapshot>,
    /// Rate snapshot recorded at collection time, read by the model rate panel of the overview.
    #[serde(skip)]
    pub rates: Option<RateSnapshot>,
}

pub(super) const COLUMNS: &str = "id,request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,session_file,entry_id,duration_ms,ttft_ms,input_tokens,output_tokens,cached_tokens,status,stop_reason,error_message,cache_write_tokens,cost_json,rate_json,reasoning_tokens";
pub(super) const COLUMN_COUNT: usize = 24;
pub(super) fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: r.get(0)?,
        request_key: r.get(1)?,
        timestamp: r.get(2)?,
        model: r.get(3)?,
        provider: r.get(4)?,
        model_reference: r.get(5)?,
        api: r.get(6)?,
        project: r.get(7)?,
        cwd: r.get(8)?,
        purpose: r.get(9)?,
        session_file: r.get(10)?,
        entry_id: r.get(11)?,
        duration_ms: r.get(12)?,
        ttft_ms: r.get(13)?,
        input_tokens: r.get(14)?,
        output_tokens: r.get(15)?,
        cached_tokens: r.get(16)?,
        status: r.get(17)?,
        stop_reason: r.get(18)?,
        error_message: r.get(19)?,
        cache_write_tokens: r.get(20)?,
        // A corrupt row snapshot only counts as unpriced; it must not fail the whole page query.
        cost: r
            .get::<_, Option<String>>(21)?
            .and_then(|json| serde_json::from_str(&json).ok()),
        rates: r
            .get::<_, Option<String>>(22)?
            .and_then(|json| serde_json::from_str(&json).ok()),
        reasoning_tokens: r.get(23)?,
    })
}

impl RequestRow {
    pub fn cost(&self) -> Option<CostSnapshot> {
        self.cost.clone()
    }
    pub fn rates(&self) -> Option<RateSnapshot> {
        self.rates.clone()
    }
    /// Request JSON shared by the list and the detail: `cost` is the snapshot total and
    /// `cost_components` holds the four components.
    pub fn value(&self) -> Result<Value> {
        let mut value = serde_json::to_value(self)?;
        let cost = self.cost();
        value["cost"] = json!(cost.as_ref().map(CostSnapshot::total));
        value["cost_components"] = json!(cost);
        Ok(value)
    }
}

/// SQL of the list query; the last column is `has_context` (whether a first-request snapshot was
/// collected).
pub(super) fn latest_sql() -> String {
    format!(
        "SELECT {COLUMNS},EXISTS(SELECT 1 FROM request_contexts WHERE request_id=requests.id AND request_json IS NOT NULL) FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC LIMIT ?3"
    )
}
