use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TokenRates {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl Default for TokenRates {
    fn default() -> Self {
        Self {
            input: 200.0e-6,
            cache_read: 10.0e-6,
            cache_write: 0.0,
            output: 1000.0e-6,
        }
    }
}

pub(crate) fn cold_prefix_rate(cache_write_tokens: u64, rates: TokenRates) -> f64 {
    if cache_write_tokens > 0 && rates.cache_write > 0.0 {
        rates.cache_write
    } else {
        rates.input
    }
}

pub(crate) fn extract_token_rates(usage: &Value, model: Option<&str>) -> TokenRates {
    let mut rates = model
        .filter(|m| !m.is_empty() && *m != "unknown" && *m != "-")
        .map(|m| rates_for_openai_or_generic(m, 0, 0))
        .unwrap_or_default();
    if let Some(details) = usage
        .get("copilotUsage")
        .and_then(|cu| cu.get("tokenDetails"))
        .and_then(Value::as_array)
    {
        for item in details {
            let batch = item
                .get("batchSize")
                .and_then(Value::as_f64)
                .unwrap_or(1_000_000.0);
            let cost_nano = item.get("costPerBatch").and_then(Value::as_f64).unwrap_or(0.0);
            if batch <= 0.0 {
                continue;
            }
            let rate = (cost_nano / 1_000_000_000.0) / batch;
            match item.get("tokenType").and_then(Value::as_str) {
                Some("input") => rates.input = rate,
                Some("cache_read") => rates.cache_read = rate,
                Some("cache_write") => rates.cache_write = rate,
                Some("output") => rates.output = rate,
                _ => {}
            }
        }
    }
    rates
}

/// Computes Claude generation-aware rates and weights `ephemeral_1h` (2.0x) vs `ephemeral_5m` (1.25x) cache writes.
pub(crate) fn rates_for_claude_turn(
    model: &str,
    cw_total: u64,
    cw_5m: u64,
    cw_1h: u64,
    is_subagent: bool,
) -> TokenRates {
    let lower = model.to_ascii_lowercase();
    let (base_in, base_cr, base_out) = if lower.contains("opus-4-1")
        || lower.contains("opus-4-0")
        || lower.contains("opus-4-2025")
        || lower.contains("claude-3-opus")
    {
        (1500.0e-6, 150.0e-6, 7500.0e-6)
    } else if lower.contains("opus") {
        (500.0e-6, 50.0e-6, 2500.0e-6)
    } else if lower.contains("haiku") {
        (100.0e-6, 10.0e-6, 500.0e-6)
    } else {
        (300.0e-6, 30.0e-6, 1500.0e-6)
    };

    let rate_5m = base_in * 1.25;
    let rate_1h = base_in * 2.0;
    let cache_write = if cw_5m > 0 || cw_1h > 0 {
        let accounted = cw_5m.saturating_add(cw_1h);
        let remainder = cw_total.saturating_sub(accounted);
        let rem_rate = if is_subagent { rate_5m } else { rate_1h };
        let denom = cw_total.max(accounted) as f64;
        if denom > 0.0 {
            ((cw_5m as f64) * rate_5m + (cw_1h as f64) * rate_1h + (remainder as f64) * rem_rate)
                / denom
        } else if cw_1h > 0 {
            rate_1h
        } else {
            rate_5m
        }
    } else {
        rate_5m
    };

    TokenRates {
        input: base_in,
        cache_read: base_cr,
        cache_write,
        output: base_out,
    }
}

/// Computes OpenAI / Codex / multi-model fallback rates including 2026 GPT-5.6+ (1.25x/2.5x write)
/// and GPT-6.1 Sol/Terra (0.05x read, 1.25x/2.5x write) pricing tiers.
pub(crate) fn rates_for_openai_or_generic(model: &str, cw_total: u64, cw_12h: u64) -> TokenRates {
    let lower = model.to_ascii_lowercase();
    if lower.contains("claude") || lower.contains("opus") || lower.contains("sonnet") || lower.contains("haiku") {
        return rates_for_claude_turn(model, cw_total, cw_total, 0, false);
    }
    if lower.contains("gpt-6") {
        let base_in = 200.0e-6;
        let cw_30m = base_in * 1.25;
        let cw_long = base_in * 2.5;
        let cw = weighted_write_rate(cw_total, cw_12h, cw_30m, cw_long);
        return TokenRates {
            input: base_in,
            cache_read: 10.0e-6, // 0.05x (95% discount on GPT-6.1 Sol/Terra)
            cache_write: cw,
            output: 1000.0e-6,
        };
    }
    if lower.starts_with("gpt-5.6") || lower.contains("gpt-5.6-") || lower.starts_with("gpt-5.7") {
        let base_in = if lower.contains("mini") || lower.contains("nano") || lower.contains("fast") {
            60.0e-6
        } else {
            250.0e-6
        };
        let cw_30m = base_in * 1.25;
        let cw_long = base_in * 2.5;
        let cw = weighted_write_rate(cw_total, cw_12h, cw_30m, cw_long);
        return TokenRates {
            input: base_in,
            cache_read: base_in * 0.10,
            cache_write: cw,
            output: base_in * 5.0,
        };
    }
    if lower.contains("codex-mini") || lower.contains("gpt-5-mini") || lower.contains("gpt-5-nano") {
        return TokenRates {
            input: 50.0e-6,
            cache_read: 5.0e-6,
            cache_write: 0.0,
            output: 200.0e-6,
        };
    }
    if lower.contains("gpt-5") || lower.contains("codex") {
        return TokenRates {
            input: 250.0e-6,
            cache_read: 25.0e-6, // 0.10x (90% discount)
            cache_write: 0.0,
            output: 1000.0e-6,
        };
    }
    if lower.contains("gpt-4.1") || lower.starts_with("o3") || lower.starts_with("o4-mini") {
        return TokenRates {
            input: 200.0e-6,
            cache_read: 50.0e-6, // 0.25x (75% discount)
            cache_write: 0.0,
            output: 800.0e-6,
        };
    }
    if lower.contains("gpt-4o") || lower.starts_with("o1") {
        return TokenRates {
            input: 250.0e-6,
            cache_read: 125.0e-6, // 0.50x (50% discount)
            cache_write: 0.0,
            output: 1000.0e-6,
        };
    }
    if lower.contains("gemini") && lower.contains("flash") {
        return TokenRates {
            input: 50.0e-6,
            cache_read: 5.0e-6,
            cache_write: 0.0,
            output: 200.0e-6,
        };
    }
    TokenRates::default()
}

fn weighted_write_rate(cw_total: u64, cw_12h: u64, short_rate: f64, long_rate: f64) -> f64 {
    if cw_12h == 0 {
        return short_rate;
    }
    let total = cw_total.max(cw_12h);
    let short_tok = total.saturating_sub(cw_12h);
    ((short_tok as f64) * short_rate + (cw_12h as f64) * long_rate) / (total as f64)
}

/// Legacy GitHub Copilot monthly Premium Request model multiplier (for Pro 300 / Pro+ 1500 plans).
pub(crate) fn premium_request_multiplier(model: &str) -> f64 {
    let lower = model.to_ascii_lowercase();
    if lower == "gpt-4.1"
        || lower == "gpt-4o"
        || lower == "gpt-5-mini"
        || lower.starts_with("mai-code-1-flash")
        || lower.starts_with("mai-code-1.1-flash")
        || lower.starts_with("mai-code-2-flash")
    {
        return 0.0;
    }
    if lower.contains("opus") {
        return 3.0;
    }
    if lower.contains("haiku")
        || lower.contains("flash")
        || lower.contains("o4-mini")
        || lower.contains("codex-mini")
    {
        return 0.33;
    }
    1.0
}
