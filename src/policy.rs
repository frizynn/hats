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
    /// The user disabled the account in use.
    Disabled,
    /// The provider refused the login of the account in use.
    Login,
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

/// Whether auto switching may move from `current` to `c`: another account,
/// signed in and not turned off.
fn eligible(c: &Candidate, current: &str, config: &HarnessConfig) -> bool {
    c.logged_in && c.label != current && !config.disabled.contains(&c.label)
}

/// Whether any account but `current` could be switched to, room aside.
pub fn has_destination(current: &str, candidates: &[Candidate], config: &HarnessConfig) -> bool {
    candidates.iter().any(|c| eligible(c, current, config))
}

/// The account to switch to from `current`, or `None` if no account has
/// enough room. An account never seen may be the spent one, so it is taken
/// only when no account with data has room, the first in the order.
pub fn pick<'a>(
    current: &str,
    candidates: &'a [Candidate],
    config: &HarnessConfig,
) -> Option<&'a str> {
    let ceiling = f64::from(config.threshold) - MARGIN;
    let ring = rotation(candidates.iter().map(|c| c.label.as_str()), &config.order);
    // `next` walks the order from the account after the current one.
    let start = match config.strategy {
        Strategy::Next => ring.iter().position(|l| *l == current).map_or(0, |i| i + 1),
        Strategy::Headroom => 0,
    };
    let others: Vec<&Candidate> = (0..ring.len())
        .map(|i| ring[(start + i) % ring.len()])
        .filter_map(|l| candidates.iter().find(|c| c.label == l))
        .filter(|c| eligible(c, current, config))
        .collect();
    let with_room = others
        .iter()
        .filter_map(|c| Some((*c, c.used.filter(|u| *u <= ceiling)?)));
    let seen = match config.strategy {
        Strategy::Next => with_room.map(|(c, _)| c).next(),
        // The first of equals, so ties follow the order.
        Strategy::Headroom => with_room.min_by(|a, b| a.1.total_cmp(&b.1)).map(|(c, _)| c),
    };
    seen.or_else(|| others.iter().copied().find(|c| c.used.is_none()))
        .map(|c| c.label.as_str())
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
    fn an_account_turned_off_is_never_picked() {
        let accounts = [
            c("a", true, Some(97.0)),
            c("b", true, Some(10.0)),
            c("c", true, Some(50.0)),
            c("d", true, None),
        ];
        let off = |strategy, labels: &[&str]| HarnessConfig {
            disabled: labels.iter().map(|s| s.to_string()).collect(),
            ..config(strategy, &[])
        };
        for strategy in [Strategy::Headroom, Strategy::Next] {
            assert_eq!(pick("a", &accounts, &off(strategy, &["b"])), Some("c"));
            // Off with room comes after an account never seen.
            assert_eq!(pick("a", &accounts, &off(strategy, &["b", "c"])), Some("d"));
            // Every other account off: stay.
            assert_eq!(pick("a", &accounts, &off(strategy, &["b", "c", "d"])), None);
            // The current account being off does not stop it from leaving.
            assert_eq!(pick("a", &accounts, &off(strategy, &["a"])), Some("b"));
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
