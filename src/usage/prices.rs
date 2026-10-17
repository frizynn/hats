//! What tokens would cost on the API at list price, in USD per million. This
//! is an API-equivalent figure, not a bill: Claude and ChatGPT plans are
//! flat-rate. The scan keeps tokens only, so a price change needs no rescan.
//!
//! Anthropic: <https://platform.claude.com/docs/en/about-claude/pricing>, read
//! 2026-10-16. Fast mode (Opus 5.5, Opus 5, Opus 4.8) doubles input and
//! output, and the cache multipliers apply on top. Haiku 5.5 prices a request
//! whose prompt (input, cache reads and writes) is over 100,000 tokens at the
//! higher tier; the scan marks those requests `+long`.
//!
//! OpenAI: <https://developers.openai.com/api/docs/pricing>, read 2026-10-16.
//! Codex reports cached input (priced as the cache read) and no cache-write
//! split. `codex-auto-review` is not on the page: priced as gpt-5.6-luna, as
//! ccusage does.

use super::Tokens;

pub const PRICES_CHECKED: &str = "2026-10-16";

/// USD for these tokens, or `None` when the model has no price.
pub fn cost(_model: &str, _t: &Tokens) -> Option<f64> {
    None
}

/// `claude-opus-5-5+fast` to `Opus 5.5 · fast`; ids it does not know pass
/// through.
pub fn model_label(model: &str) -> String {
    model.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u64 = 1_000_000;

    fn tokens(input: u64, write_5m: u64, write_1h: u64, read: u64, output: u64) -> Tokens {
        Tokens {
            requests: 1,
            input,
            output,
            cache_write_5m: write_5m,
            cache_write_1h: write_1h,
            cache_read: read,
            reasoning: 0,
        }
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn list_price_per_model_and_tier() {
        assert!(close(
            cost("claude-opus-5-5", &tokens(M, M, M, M, M)),
            4.0 + 5.0 + 8.0 + 0.2 + 20.0
        ));
        assert!(close(
            cost("claude-opus-5-5+fast", &tokens(M, 0, 0, 0, M)),
            2.0 * (4.0 + 20.0)
        ));
        // Fast mode only doubles the models that have it.
        assert!(close(
            cost("claude-sonnet-5-5+fast", &tokens(M, 0, 0, 0, 0)),
            2.0
        ));
        assert!(close(
            cost("claude-haiku-4-5-20251001", &tokens(M, 0, 0, 0, 0)),
            1.0
        ));
        assert!(close(
            cost("claude-haiku-5-5+long", &tokens(M, 0, 0, 0, 0)),
            0.5
        ));
        assert!(close(
            cost("claude-haiku-5-5-20261001+long", &tokens(M, 0, 0, 0, 0)),
            0.5
        ));
        assert!(close(
            cost("gpt-6.1-sol", &tokens(M, 0, 0, M, M)),
            2.0 + 0.1 + 10.0
        ));
        assert_eq!(cost("mystery-model", &tokens(M, 0, 0, 0, 0)), None);
    }

    #[test]
    fn labels() {
        let labels: Vec<String> = [
            "claude-opus-5-5",
            "claude-opus-5-5+fast",
            "gpt-6.1-sol",
            "claude-haiku-4-5-20251001",
            "claude-haiku-5-5+long",
            "codex-auto-review",
            "mystery",
        ]
        .into_iter()
        .map(model_label)
        .collect();
        assert_eq!(
            labels,
            [
                "Opus 5.5",
                "Opus 5.5 · fast",
                "GPT-6.1 Sol",
                "Haiku 4.5",
                "Haiku 5.5 · long prompt",
                "Auto review",
                "mystery"
            ]
        );
    }
}
