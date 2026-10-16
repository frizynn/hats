//! When to leave an account and which one to take. Pure: the callers bring
//! the readings and apply the decision.

use serde::{Deserialize, Serialize};

use crate::config::{HarnessConfig, Strategy};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// The user chose the account.
    Manual,
    /// The account passed the configured threshold.
    Threshold,
    /// The account reached its limit.
    Limit,
}

/// A candidate must sit this many points under the threshold, so two accounts
/// near the line do not trade places on every repaint.
pub const MARGIN: f64 = 10.0;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub label: String,
    pub logged_in: bool,
    /// Highest use across the limit windows, or `None` if never seen.
    pub used: Option<f64>,
}

/// Why `used` should make the account step aside, if it should.
pub fn trigger(used: f64, threshold: u8) -> Option<Reason> {
    if used >= 100.0 {
        Some(Reason::Limit)
    } else if used >= f64::from(threshold) {
        Some(Reason::Threshold)
    } else {
        None
    }
}

/// Labels in rotation order: `order` first, then the rest by name.
pub fn rotation<'a>(labels: impl IntoIterator<Item = &'a str>, order: &[String]) -> Vec<&'a str> {
    let mut labels: Vec<&str> = labels.into_iter().collect();
    labels.sort_by_key(|l| (order.iter().position(|o| o == l).unwrap_or(usize::MAX), *l));
    labels
}

/// The account to switch to from `current`, or `None` if no account has
/// enough room. An account never seen counts as unused.
pub fn pick<'a>(
    current: &str,
    candidates: &'a [Candidate],
    config: &HarnessConfig,
) -> Option<&'a str> {
    let ceiling = f64::from(config.threshold) - MARGIN;
    let eligible =
        |c: &Candidate| c.logged_in && c.label != current && c.used.unwrap_or(0.0) <= ceiling;
    let ring = rotation(candidates.iter().map(|c| c.label.as_str()), &config.order);
    let by_label = |label: &str| {
        candidates
            .iter()
            .find(|c| c.label == label)
            .expect("label from candidates")
    };
    match config.strategy {
        Strategy::Next => {
            let start = ring.iter().position(|l| *l == current).map_or(0, |i| i + 1);
            (0..ring.len())
                .map(|i| ring[(start + i) % ring.len()])
                .find(|l| eligible(by_label(l)))
        }
        Strategy::Headroom => ring
            .iter()
            .copied()
            .filter(|l| eligible(by_label(l)))
            .min_by(|a, b| {
                let used = |l: &str| by_label(l).used.unwrap_or(0.0);
                used(a).total_cmp(&used(b))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(label: &str, logged_in: bool, used: Option<f64>) -> Candidate {
        Candidate {
            label: label.to_owned(),
            logged_in,
            used,
        }
    }

    fn config(strategy: Strategy, order: &[&str]) -> HarnessConfig {
        HarnessConfig {
            strategy,
            order: order.iter().map(|s| s.to_string()).collect(),
            ..HarnessConfig::default()
        }
    }

    #[test]
    fn triggers() {
        assert_eq!(trigger(89.9, 90), None);
        assert_eq!(trigger(90.0, 90), Some(Reason::Threshold));
        assert_eq!(trigger(100.0, 90), Some(Reason::Limit));
        assert_eq!(trigger(100.0, 100), Some(Reason::Limit));
    }

    #[test]
    fn rotation_puts_order_first() {
        let order = vec!["zeta".to_owned(), "beta".to_owned()];
        assert_eq!(
            rotation(["alpha", "beta", "gamma", "zeta"], &order),
            ["zeta", "beta", "alpha", "gamma"]
        );
    }

    #[test]
    fn headroom_takes_the_emptiest_eligible_account() {
        let accounts = [
            c("a", true, Some(95.0)),
            c("b", true, Some(70.0)),
            c("c", true, Some(20.0)),
            c("d", false, Some(0.0)),
            c("e", true, Some(85.0)),
        ];
        assert_eq!(
            pick("a", &accounts, &config(Strategy::Headroom, &[])),
            Some("c")
        );
    }

    #[test]
    fn next_follows_the_order_and_wraps() {
        let accounts = [
            c("a", true, Some(50.0)),
            c("b", true, Some(95.0)),
            c("c", true, Some(81.0)),
            c("d", true, None),
        ];
        let cfg = config(Strategy::Next, &["c", "b", "d", "a"]);
        // From b: c is over the margin (81 > 80) and d was never seen, so a,
        // wrapping around to the start of the order.
        assert_eq!(pick("b", &accounts, &cfg), Some("a"));
        // From a, nobody else with data has room: d, which was never seen.
        assert_eq!(pick("a", &accounts, &cfg), Some("d"));
    }

    #[test]
    fn an_account_never_seen_only_when_none_with_data_has_room() {
        // An account never seen may be spent: the one with data goes first.
        let accounts = [
            c("a", true, Some(97.0)),
            c("b", true, None),
            c("c", true, Some(60.0)),
            c("d", true, None),
        ];
        for strategy in [Strategy::Headroom, Strategy::Next] {
            assert_eq!(pick("a", &accounts, &config(strategy, &[])), Some("c"));
        }
        // Nobody with data has room: the first never seen, in the order.
        let accounts = [
            c("a", true, Some(97.0)),
            c("b", true, None),
            c("c", true, Some(85.0)),
            c("d", true, None),
        ];
        for strategy in [Strategy::Headroom, Strategy::Next] {
            let cfg = config(strategy, &["a", "d", "b", "c"]);
            assert_eq!(pick("a", &accounts, &cfg), Some("d"));
        }
    }

    #[test]
    fn nobody_with_room_means_stay() {
        let accounts = [
            c("a", true, Some(95.0)),
            c("b", true, Some(85.0)),
            c("c", false, None),
        ];
        for strategy in [Strategy::Headroom, Strategy::Next] {
            assert_eq!(pick("a", &accounts, &config(strategy, &[])), None);
        }
    }
}
