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

/// Input, cache write 5 min, cache write 1 h, cache read, output.
type Rates = [f64; 5];

const PRICES: &[(&str, Rates)] = &[
    ("claude-fable-5-1", [10.0, 12.5, 20.0, 0.25, 50.0]),
    ("claude-mythos-5-1", [10.0, 12.5, 20.0, 0.25, 50.0]),
    ("claude-fable-5", [10.0, 12.5, 20.0, 1.0, 50.0]),
    ("claude-mythos-5", [10.0, 12.5, 20.0, 1.0, 50.0]),
    ("claude-opus-5-5", [4.0, 5.0, 8.0, 0.2, 20.0]),
    ("claude-opus-5", [5.0, 6.25, 10.0, 0.5, 25.0]),
    ("claude-opus-4-8", [5.0, 6.25, 10.0, 0.5, 25.0]),
    ("claude-opus-4-7", [5.0, 6.25, 10.0, 0.5, 25.0]),
    ("claude-opus-4-6", [5.0, 6.25, 10.0, 0.5, 25.0]),
    ("claude-opus-4-5", [5.0, 6.25, 10.0, 0.5, 25.0]),
    ("claude-opus-4-1", [15.0, 18.75, 30.0, 1.5, 75.0]),
    ("claude-opus-4", [15.0, 18.75, 30.0, 1.5, 75.0]),
    ("claude-sonnet-5-5", [2.0, 2.5, 4.0, 0.1, 10.0]),
    ("claude-sonnet-5", [2.0, 2.5, 4.0, 0.2, 10.0]),
    ("claude-sonnet-4-6", [3.0, 3.75, 6.0, 0.3, 15.0]),
    ("claude-sonnet-4-5", [3.0, 3.75, 6.0, 0.3, 15.0]),
    ("claude-sonnet-4", [3.0, 3.75, 6.0, 0.3, 15.0]),
    ("claude-haiku-5-5", [0.1, 0.125, 0.2, 0.01, 0.5]),
    ("claude-haiku-5-5+long", [0.5, 0.625, 1.0, 0.05, 2.5]),
    ("claude-haiku-4-5", [1.0, 1.25, 2.0, 0.1, 5.0]),
    ("claude-haiku-3-5", [0.8, 1.0, 1.6, 0.08, 4.0]),
    ("gpt-6.1-sol", [2.0, 2.5, 2.5, 0.1, 10.0]),
    ("gpt-6-luna", [0.1, 0.125, 0.125, 0.01, 0.5]),
    ("gpt-5.6-luna", [0.2, 0.25, 0.25, 0.02, 1.2]),
    ("gpt-5.6-sol", [4.0, 5.0, 5.0, 0.4, 20.0]),
    ("codex-auto-review", [0.2, 0.25, 0.25, 0.02, 1.2]),
];

const FAST: &[&str] = &["claude-opus-5-5", "claude-opus-5", "claude-opus-4-8"];

/// `claude-opus-5-5+fast` to (`claude-opus-5-5`, `+fast`); a dated id
/// (`claude-haiku-4-5-20251001`) to its family.
fn split(model: &str) -> (&str, &str) {
    let (base, tier) = match model.rfind('+') {
        Some(i) if matches!(&model[i..], "+fast" | "+long") => model.split_at(i),
        _ => (model, ""),
    };
    let family = match base.rsplit_once('-') {
        Some((family, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => {
            family
        }
        _ => base,
    };
    (family, tier)
}

/// USD for these tokens, or `None` when the model has no price.
pub fn cost(model: &str, t: &Tokens) -> Option<f64> {
    let (family, tier) = split(model);
    let rates = |name: &str| PRICES.iter().find(|(m, _)| *m == name).map(|(_, r)| r);
    let [input, write_5m, write_1h, read, output] = *match tier {
        "+long" => rates(&format!("{family}+long"))?,
        _ => rates(family)?,
    };
    let factor = if tier == "+fast" && FAST.contains(&family) {
        2.0
    } else {
        1.0
    };
    let usd = t.input as f64 * input
        + t.cache_write_5m as f64 * write_5m
        + t.cache_write_1h as f64 * write_1h
        + t.cache_read as f64 * read
        + t.output as f64 * output;
    Some(factor * usd / 1e6)
}

/// `claude-opus-5-5+fast` to `Opus 5.5 · fast`; ids it does not know pass
/// through.
pub fn model_label(model: &str) -> String {
    let (family, tier) = split(model);
    let mut label = if let Some(rest) = family.strip_prefix("claude-") {
        let mut parts = rest.split('-');
        let name = parts.next().unwrap_or_default();
        let version: Vec<&str> = parts.collect();
        let mut chars = name.chars();
        match chars.next() {
            Some(first) if version.iter().all(|v| v.parse::<u32>().is_ok()) => {
                format!(
                    "{}{} {}",
                    first.to_uppercase(),
                    chars.as_str(),
                    version.join(".")
                )
            }
            _ => family.to_owned(),
        }
    } else if let Some(rest) = family.strip_prefix("gpt-") {
        match rest.split_once('-') {
            Some((version, name)) => {
                let mut chars = name.chars();
                let first = chars.next().map(|c| c.to_uppercase().to_string());
                format!(
                    "GPT-{version} {}{}",
                    first.unwrap_or_default(),
                    chars.as_str()
                )
            }
            None => format!("GPT-{rest}"),
        }
    } else if family == "codex-auto-review" {
        "Auto review".to_owned()
    } else {
        family.to_owned()
    };
    match tier {
        "+fast" => label.push_str(" · fast"),
        "+long" => label.push_str(" · long prompt"),
        _ => {}
    }
    label
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
