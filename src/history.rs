//! Each Claude account's weekly use over time, so the estimate can say how
//! much the accounts spend together in a week. `limits.json` holds only the
//! last reading; this keeps every change of a `seven_day` window.
//!
//! `history.jsonl`, one line per change:
//!
//! ```json
//! {"at":1794442674,"account":"0123456789abcdef","label":"work","used":31.0,"resetsAt":1794862799}
//! ```
//!
//! Lines older than `KEEP` are dropped once the file passes `TRIM_AT`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::limits::{LimitWindow, unix_secs};
use crate::store::Store;

pub const FILE_NAME: &str = "history.jsonl";
/// Eight days: a full week back, plus the day before it for its baseline.
const KEEP: i64 = 8 * 86_400;
const TRIM_AT: u64 = 512 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Sample {
    /// Unix seconds.
    pub at: i64,
    pub account: String,
    pub label: String,
    pub used: f64,
    #[serde(rename = "resetsAt")]
    pub resets_at: i64,
}

pub fn append(
    store: &Store,
    id: &str,
    label: &str,
    weekly: &LimitWindow,
    now: SystemTime,
) -> io::Result<()> {
    let path = store.state.join(FILE_NAME);
    let sample = Sample {
        at: unix_secs(now),
        account: id.to_owned(),
        label: label.to_owned(),
        used: weekly.used_percent,
        resets_at: weekly.resets_at,
    };
    let mut line = serde_json::to_vec(&sample).expect("plain struct always serializes");
    line.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?
        .write_all(&line)?;
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > TRIM_AT {
        let keep: Vec<u8> = read(store, sample.at - KEEP)
            .iter()
            .flat_map(|s| {
                let mut l = serde_json::to_vec(s).expect("plain struct always serializes");
                l.push(b'\n');
                l
            })
            .collect();
        let tmp = path.with_extension("jsonl.tmp");
        fs::write(&tmp, keep)?;
        fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// The samples taken at `since` (Unix seconds) or later, oldest first.
pub fn read(store: &Store, since: i64) -> Vec<Sample> {
    let Ok(text) = fs::read_to_string(store.state.join(FILE_NAME)) else {
        return Vec::new();
    };
    let mut samples: Vec<Sample> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|s: &Sample| s.at >= since)
        .collect();
    samples.sort_by_key(|s| s.at);
    samples
}
