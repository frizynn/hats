//! How long the Claude accounts last together at the pace they are spent,
//! and each account's own pace this week. Weekly limits only: a 5-hour limit
//! comes back within hours and auto switching moves around it.
//!
//! The pace of all accounts comes from `history.jsonl`: everything the weekly
//! windows rose over the last week (from 0 after a reset), divided by the
//! time the history covers. With less than `MIN_HISTORY` of it, the pace is
//! what the current weekly windows hold over a week: a floor, since use from
//! before an account's last reset is not in its window.

use std::time::SystemTime;

use crate::account::Harness;
use crate::auto::Status;
use crate::config::HarnessConfig;
use crate::history;
use crate::history::Sample;
use crate::limits::{Gift, LimitWindow, unix_secs};
use crate::store::Store;

const HOUR: i64 = 3600;
pub const WEEK: i64 = 7 * 24 * HOUR;
/// Less history than this is not enough to measure a pace.
const MIN_HISTORY: i64 = 24 * HOUR;
/// Two readings of one window may disagree on its reset by about a second.
const SAME_WINDOW: i64 = HOUR;
const STEP: i64 = 15 * 60;

/// What the accounts spend together, and how long they last.
#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    /// Percent of one account's weekly limit spent per hour, all accounts.
    pub per_hour: f64,
    /// Seconds of history the pace comes from; `None` when it is the floor
    /// read from the current windows.
    pub measured_over: Option<i64>,
    /// Accounts auto switching can use.
    pub accounts: usize,
    /// Room left under the threshold now, in accounts.
    pub room: f64,
    /// Unix seconds when every usable account would sit at the threshold,
    /// within a week; `None` when it lasts the week.
    pub runs_out: Option<i64>,
    /// Unused gifts that clear the weekly limit.
    pub full_gifts: u32,
    /// `runs_out` when those gifts are used as accounts fill.
    pub runs_out_with_gifts: Option<i64>,
}

impl Fleet {
    /// Accounts a week at this pace.
    pub fn accounts_a_week(&self) -> f64 {
        self.per_hour * (WEEK / HOUR) as f64 / 100.0
    }
}

/// One account's pace in its current week.
#[derive(Debug, Clone, PartialEq)]
pub struct Pace {
    pub per_hour: f64,
    /// Where it ends its week at that pace, at most 100.
    pub at_reset: f64,
    /// Unix seconds when it reaches the threshold at that pace, before its reset.
    pub full_at: Option<i64>,
}

fn weekly(s: &Status) -> Option<&LimitWindow> {
    s.reading
        .as_ref()?
        .windows
        .iter()
        .find(|w| w.name == "seven_day")
}

/// The account's pace since its week began; `None` without a current weekly
/// window or under an hour into it.
pub fn pace(s: &Status, threshold: u8, now: SystemTime) -> Option<Pace> {
    let w = weekly(s)?;
    let now = unix_secs(now);
    let left = w.resets_at - now;
    let elapsed = WEEK - left;
    if left <= 0 || elapsed < HOUR {
        return None;
    }
    let per_hour = w.used_percent / (elapsed as f64 / HOUR as f64);
    let at_reset = (w.used_percent + per_hour * left as f64 / HOUR as f64).min(100.0);
    let threshold = f64::from(threshold);
    let full_at = (per_hour > 0.0 && w.used_percent < threshold && at_reset >= threshold)
        .then(|| now + ((threshold - w.used_percent) / per_hour * HOUR as f64) as i64);
    Some(Pace {
        per_hour,
        at_reset,
        full_at,
    })
}

/// Percent spent by all accounts over the last week of `samples`, and the
/// seconds it covers.
pub fn spent(samples: &[Sample], now: i64) -> (f64, i64) {
    let since = now - WEEK;
    let first = samples.iter().map(|s| s.at).min().unwrap_or(now).max(since);
    let mut ids: Vec<&str> = samples.iter().map(|s| s.account.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut total = 0.0;
    for id in ids {
        let mine: Vec<&Sample> = samples.iter().filter(|s| s.account == id).collect();
        for pair in mine.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if b.at < since {
                continue;
            }
            if (b.resets_at - a.resets_at).abs() < SAME_WINDOW {
                total += (b.used - a.used).max(0.0);
            } else if b.resets_at > a.resets_at {
                total += b.used;
            }
        }
    }
    (total, now - first)
}

struct Seat {
    used: f64,
    resets_at: i64,
    gifts: u32,
}

/// Steps the accounts forward, spending `per_hour` on the one with the most
/// room as auto switching does, and returns when none has room left.
fn run_out(
    mut seats: Vec<Seat>,
    per_hour: f64,
    threshold: f64,
    gifts: bool,
    now: i64,
) -> Option<i64> {
    let per_step = per_hour * STEP as f64 / HOUR as f64;
    let mut t = now;
    while t < now + WEEK {
        t += STEP;
        for s in &mut seats {
            if s.resets_at <= t {
                s.used = 0.0;
                s.resets_at += WEEK;
            }
        }
        let mut need = per_step;
        while need > 1e-9 {
            if gifts
                && seats.iter().all(|s| s.used >= threshold)
                && let Some(s) = seats.iter_mut().find(|s| s.gifts > 0)
            {
                s.gifts -= 1;
                s.used = 0.0;
            }
            let best = seats
                .iter_mut()
                .filter(|s| s.used < threshold)
                .max_by(|a, b| (threshold - a.used).total_cmp(&(threshold - b.used)));
            let Some(best) = best else {
                return Some(t);
            };
            let take = need.min(threshold - best.used);
            best.used += take;
            need -= take;
        }
    }
    None
}

fn full_gifts(gifts: &[Gift], now: i64) -> u32 {
    gifts
        .iter()
        .filter(|g| g.clears_weekly() && (g.ends_at == 0 || g.ends_at > now))
        .map(|g| g.left)
        .sum()
}

/// The estimate for the accounts in `statuses` that auto switching can use;
/// `None` without any weekly reading.
pub fn fleet(
    statuses: &[Status],
    config: &HarnessConfig,
    samples: &[Sample],
    now: SystemTime,
) -> Option<Fleet> {
    let now = unix_secs(now);
    let threshold = f64::from(config.threshold);
    let usable: Vec<&Status> = statuses
        .iter()
        .filter(|s| s.id.is_some() && !s.login_refused())
        .filter(|s| !config.disabled.contains(&s.account.label))
        .collect();
    let seats: Vec<Seat> = usable
        .iter()
        .map(|s| {
            let gifts = full_gifts(
                &s.reading
                    .as_ref()
                    .map(|r| r.gifts.clone())
                    .unwrap_or_default(),
                now,
            );
            match weekly(s).filter(|w| w.resets_at > now) {
                Some(w) => Seat {
                    used: w.used_percent,
                    resets_at: w.resets_at,
                    gifts,
                },
                // A window that reset, or none yet: empty for a whole week.
                None => Seat {
                    used: 0.0,
                    resets_at: now + WEEK,
                    gifts,
                },
            }
        })
        .collect();
    if statuses.iter().all(|s| weekly(s).is_none()) {
        return None;
    }
    let (total, covered) = spent(samples, now);
    let (per_hour, measured_over) = if covered >= MIN_HISTORY {
        (total / (covered as f64 / HOUR as f64), Some(covered))
    } else {
        let held: f64 = statuses
            .iter()
            .filter_map(weekly)
            .filter(|w| w.resets_at > now)
            .map(|w| w.used_percent)
            .sum();
        (held / (WEEK / HOUR) as f64, None)
    };
    let room = seats
        .iter()
        .map(|s| (threshold - s.used).max(0.0))
        .sum::<f64>()
        / 100.0;
    let full_gifts = seats.iter().map(|s| s.gifts).sum();
    let copy = |s: &Seat| Seat {
        used: s.used,
        resets_at: s.resets_at,
        gifts: s.gifts,
    };
    let runs_out = run_out(
        seats.iter().map(copy).collect(),
        per_hour,
        threshold,
        false,
        now,
    );
    let runs_out_with_gifts = runs_out.and(run_out(seats, per_hour, threshold, true, now));
    Some(Fleet {
        per_hour,
        measured_over,
        accounts: usable.len(),
        room,
        runs_out,
        full_gifts,
        runs_out_with_gifts,
    })
}

/// The estimate from the stored history; Claude Code only.
pub fn read(
    store: &Store,
    harness: Harness,
    statuses: &[Status],
    config: &HarnessConfig,
    now: SystemTime,
) -> Option<Fleet> {
    if harness != Harness::Claude {
        return None;
    }
    let samples = history::read(store, unix_secs(now) - WEEK - 86_400);
    fleet(statuses, config, &samples, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: i64, account: &str, used: f64, resets_at: i64) -> Sample {
        Sample {
            at,
            account: account.to_owned(),
            label: account.to_owned(),
            used,
            resets_at,
        }
    }

    #[test]
    fn spent_adds_rises_and_restarts_from_zero_after_a_reset() {
        let now = 10 * WEEK;
        let samples = [
            sample(now - 3 * 86_400, "a", 50.0, now + 86_400),
            sample(now - 2 * 86_400, "a", 70.0, now + 86_401),
            sample(now - 86_400, "b", 90.0, now - 3600),
            // b reset: its new week holds 10.
            sample(now - 3600, "b", 10.0, now - 3600 + WEEK),
        ];
        let (total, covered) = spent(&samples, now);
        assert_eq!(total, 20.0 + 10.0);
        assert_eq!(covered, 3 * 86_400);
    }

    #[test]
    fn runs_out_when_the_pace_beats_the_room() {
        let now = 0;
        let seats = || {
            vec![
                Seat {
                    used: 50.0,
                    resets_at: now + 5 * 86_400,
                    gifts: 1,
                },
                Seat {
                    used: 90.0,
                    resets_at: now + 6 * 86_400,
                    gifts: 0,
                },
            ]
        };
        // 47 + 7 points of room at 1 per hour: out after 54 h, within a step.
        let out = run_out(seats(), 1.0, 97.0, false, now).unwrap();
        assert!((54 * 3600..=54 * 3600 + STEP).contains(&out), "{out}");
        // A gift adds 97 more points, past the first reset at 120 h.
        assert!(run_out(seats(), 1.0, 97.0, true, now).is_none_or(|t| t > 120 * 3600));
        // A slow pace lasts the week.
        assert_eq!(run_out(seats(), 0.2, 97.0, false, now), None);
    }
}
