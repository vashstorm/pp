//! Optional token budget for the search body; structured metadata stays outside the budget.
use anyhow::{Result, ensure};
use pp_core::context::estimate_tokens;

const NOTICE: &str = "Output truncated by token limit; narrow paths or pattern.";

pub(super) struct Budget {
    max_tokens: Option<u64>,
    pub(super) reached: bool,
}

impl Budget {
    pub(super) fn new(max_tokens: Option<u64>) -> Result<Self> {
        ensure!(max_tokens != Some(0), "max_output_tokens must be positive");
        Ok(Self {
            max_tokens,
            reached: false,
        })
    }

    pub(super) fn enabled(&self) -> bool {
        self.max_tokens.is_some()
    }

    pub(super) fn allows(&mut self, content: &str, text: &str) -> Result<bool> {
        let Some(limit) = self.max_tokens else {
            return Ok(true);
        };
        // Reserve room for the full truncation notice; when a tiny budget cannot fit it, the structured mark reports the truncation.
        let fits = estimate_tokens(&format!("{content}{text}\n\n{NOTICE}"))? <= limit;
        self.reached |= !fits;
        Ok(fits)
    }

    pub(super) fn finish(&self, content: &mut String) -> Result<()> {
        if let Some(limit) = self.max_tokens.filter(|_| self.reached) {
            let notice = if content.is_empty() {
                NOTICE.to_owned()
            } else {
                format!("\n\n{NOTICE}")
            };
            if estimate_tokens(&format!("{content}{notice}"))? <= limit
                && content.len() + notice.len() <= super::MAX_OUTPUT
            {
                content.push_str(&notice);
            }
        }
        Ok(())
    }
}
