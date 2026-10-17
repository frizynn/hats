//! The scan: Claude Code transcripts and Codex rollouts to token totals per
//! local day, harness, account, model and project.
//!
//! Only bytes appended since the last scan are read, and only whole lines: a
//! half-written tail is read again next time. Rows are found by byte markers
//! and their fields read in place (`bytes.rs`); a row the fast path cannot
//! vouch for is parsed whole. Four threads read four logs at a time (the scan
//! waits on the disk; more threads did not help), and their rows are counted
//! in log order, so which copy of a repeated message counts does not depend
//! on which thread finished first.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::Day;
use super::accounts::Book;
use crate::account::Harness;

/// Requests, input, output, cache write 5 min, cache write 1 h, cache read,
/// reasoning: Nenu's columns.
pub type Sum = [u64; 7];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub day: Day,
    pub harness: Harness,
    pub account: u32,
    pub model: u32,
    pub repo: u32,
}

/// Daily totals, with account, model and project names kept once.
#[derive(Default)]
pub struct Totals {
    /// First day hats kept totals of its own.
    pub tracked_from: Day,
    /// Earliest day any log had.
    pub logs_from: Option<Day>,
    names: Vec<String>,
    pub sums: HashMap<Key, Sum>,
    /// Project key to folder name.
    pub repos: HashMap<String, String>,
}

impl Totals {
    pub fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }
}

/// Totals, offsets and the seen set: everything a scan reads and saves.
pub struct Tally {
    pub totals: Totals,
}

impl Tally {
    pub fn empty(today: Day) -> Self {
        Self {
            totals: Totals {
                tracked_from: today,
                ..Totals::default()
            },
        }
    }

    pub fn load(_dir: &Path, today: Day) -> Self {
        Self::empty(today)
    }

    pub fn save(&mut self, _dir: &Path, _today_utc: u16) -> io::Result<()> {
        Ok(())
    }
}

/// Where to look: each Claude config dir's `projects` (one per real
/// directory, since hats' accounts link theirs to one place) and each Codex
/// home.
#[derive(Default)]
pub struct Roots {
    /// Config dir as the switch logs name it, and its `projects` directory.
    pub claude: Vec<(String, PathBuf)>,
    pub codex: Vec<PathBuf>,
}

pub struct Input<'a> {
    pub roots: &'a Roots,
    pub book: &'a Book,
    /// Where the project walk stops.
    pub home: &'a Path,
    /// The calendar day of a Unix millisecond: local time, or a fixed zone in
    /// tests.
    pub day_of: fn(i64) -> Day,
    /// Bytes per read; a longer line is skipped through and read back only
    /// when it holds a row.
    pub chunk: usize,
    pub threads: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Stats {
    /// Logs found.
    pub files: usize,
    /// Logs read because they grew.
    pub read: usize,
    pub bytes: u64,
    /// Requests counted.
    pub rows: u64,
    /// Repeats of a request already counted.
    pub duplicates: u64,
    /// Rows parsed whole because the fast path could not vouch for them.
    pub fallback: u64,
    /// Rows that did not parse.
    pub skipped: u64,
    pub ms: u64,
}

pub fn scan(_tally: &mut Tally, _input: &Input) -> Stats {
    Stats::default()
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
