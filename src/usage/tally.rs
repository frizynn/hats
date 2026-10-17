//! What a scan counted and where each log stopped, and how both are saved
//! under `<state>/usage/`: daily totals, offsets per log, directory listings
//! and the seen set of Claude message keys.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Day;
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
    index: HashMap<String, u32>,
    pub sums: HashMap<Key, Sum>,
    /// Project key to folder name.
    pub repos: HashMap<String, String>,
}

impl Totals {
    pub fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.index.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_owned());
        self.index.insert(name.to_owned(), id);
        id
    }

    pub fn add(&mut self, key: Key, tokens: &[u64; 6]) {
        let sum = self.sums.entry(key).or_default();
        sum[0] += 1;
        for (s, t) in sum[1..].iter_mut().zip(tokens) {
            *s += t;
        }
        if self.logs_from.is_none_or(|d| key.day < d) {
            self.logs_from = Some(key.day);
        }
    }
}

/// What a scan remembers per log. Claude logs by path; Codex rollouts by id,
/// because Codex moves them to `archived_sessions/`.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct FileEntry {
    /// Bytes consumed, whole lines only.
    pub o: u64,
    /// Size at the last scan.
    pub s: u64,
    /// Project key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<String>,
    /// Codex: model, account and cumulative totals so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<[u64; 5]>,
}

/// A directory's mtime (ns), subdirectories and logs: an unchanged directory
/// is not listed again.
#[derive(Clone, Serialize, Deserialize)]
pub struct Listing(pub i64, pub Vec<String>, pub Vec<String>);

#[derive(Default, Serialize, Deserialize)]
pub struct ScanState {
    v: u32,
    pub files: HashMap<String, FileEntry>,
    pub dirs: HashMap<String, Listing>,
    /// cwd to project key.
    pub cwds: HashMap<String, String>,
}

/// Claude message keys already counted, so a response written once per
/// content block, or copied into another transcript, counts once. Open
/// addressing over two arrays: 10 bytes a slot instead of a map's entry.
pub struct Seen {
    keys: Vec<u64>,
    /// Days since 1970 (UTC) of each key, to forget old ones when saving.
    days: Vec<u16>,
    pub count: usize,
    added: usize,
}

impl Default for Seen {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl Seen {
    /// Room for `n` keys at most three quarters full.
    fn with_capacity(n: usize) -> Self {
        let size = (n * 4 / 3 + 1).next_power_of_two().max(1024);
        Self {
            keys: vec![0; size],
            days: vec![0; size],
            count: 0,
            added: 0,
        }
    }

    /// True when `key` was new and is now counted.
    pub fn add(&mut self, key: u64, day: u16) -> bool {
        let new = self.insert(key, day);
        self.added += usize::from(new);
        new
    }

    fn insert(&mut self, key: u64, day: u16) -> bool {
        let mask = self.keys.len() - 1;
        let mut i = (key.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 32) as usize & mask;
        loop {
            match self.keys[i] {
                k if k == key => return false,
                0 => break,
                _ => i = (i + 1) & mask,
            }
        }
        self.keys[i] = key;
        self.days[i] = day;
        self.count += 1;
        // At most three quarters full, so a probe stays short.
        if self.count * 4 > self.keys.len() * 3 {
            let keys = std::mem::take(&mut self.keys);
            let days = std::mem::take(&mut self.days);
            *self = Self {
                keys: vec![0; keys.len() * 2],
                days: vec![0; keys.len() * 2],
                count: 0,
                added: self.added,
            };
            for (k, d) in keys.into_iter().zip(days) {
                if k != 0 {
                    self.insert(k, d);
                }
            }
        }
        true
    }

    /// `[u32 count]` and that many `[u64 key][u16 day]`, without keys older
    /// than `keep` days before `today`.
    fn write(&self, out: &mut impl Write, today: u16, keep: u16) -> io::Result<()> {
        let cutoff = today.saturating_sub(keep);
        let live = || {
            self.keys
                .iter()
                .zip(&self.days)
                .filter(move |(k, d)| **k != 0 && **d >= cutoff)
        };
        out.write_all(&(live().count() as u32).to_le_bytes())?;
        for (k, d) in live() {
            out.write_all(&k.to_le_bytes())?;
            out.write_all(&d.to_le_bytes())?;
        }
        Ok(())
    }

    fn read(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        let mut input = io::BufReader::new(file);
        let mut n = [0; 4];
        input.read_exact(&mut n)?;
        let n = u32::from_le_bytes(n) as usize;
        if len != 4 + n as u64 * 10 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut seen = Self::with_capacity(n);
        let mut record = [0; 10];
        for _ in 0..n {
            input.read_exact(&mut record)?;
            let (k, d) = record.split_at(8);
            seen.insert(
                u64::from_le_bytes(k.try_into().expect("8 bytes")),
                u16::from_le_bytes(d.try_into().expect("2 bytes")),
            );
        }
        Ok(seen)
    }
}

/// The save these totals belong to: they were counted with
/// `scan.<gen>.json` and `seen.<seen>.bin`.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Generation {
    scan: u64,
    seen: u64,
}

/// Totals, offsets and the seen set: everything a scan reads and saves.
pub struct Tally {
    pub totals: Totals,
    pub state: ScanState,
    pub seen: Seen,
    /// The save these were loaded from, if any.
    files: Option<Generation>,
}

/// Days of message keys kept for deduplication across logs; Claude Code
/// deletes transcripts after 30.
const SEEN_DAYS: u16 = 90;

#[derive(Serialize, Deserialize)]
struct TotalsFile {
    v: u32,
    #[serde(rename = "trackedFrom")]
    tracked_from: String,
    #[serde(rename = "logsFrom", default, skip_serializing_if = "Option::is_none")]
    logs_from: Option<String>,
    /// Day to `<harness>|<account>|<model>|<project>` to sum.
    days: std::collections::BTreeMap<String, HashMap<String, Sum>>,
    repos: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    files: Option<Generation>,
}

impl TotalsFile {
    /// `totals.json`, or `None` when there is none this hats can use.
    fn read(dir: &Path) -> Result<Option<Self>, String> {
        let path = dir.join("totals.json");
        let Ok(bytes) = fs::read(&path) else {
            return Ok(None);
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(totals) if totals.v == 1 => Ok(Some(totals)),
            _ => {
                #[derive(Deserialize)]
                struct Version {
                    v: u32,
                }
                match serde_json::from_slice::<Version>(&bytes) {
                    Ok(Version { v }) if v > 1 => Err(format!(
                        "{}: saved by a newer hats (version {v}); use that one",
                        path.display()
                    )),
                    _ => Ok(None),
                }
            }
        }
    }
}

impl Tally {
    pub fn empty(today: Day) -> Self {
        Self {
            totals: Totals {
                tracked_from: today,
                ..Totals::default()
            },
            state: ScanState {
                v: 1,
                ..ScanState::default()
            },
            seen: Seen::default(),
            files: None,
        }
    }

    /// What the last save left, or an empty tally when there is none: totals
    /// without the offsets they were counted with would count logs twice.
    /// Totals a newer hats saved are refused, not replaced: they may hold
    /// days whose logs are gone.
    pub fn load(dir: &Path, today: Day) -> Result<Self, String> {
        let read = |name: &str| fs::read(dir.join(name)).ok();
        let Some(totals) = TotalsFile::read(dir)? else {
            return Ok(Self::empty(today));
        };
        let Some(files) = totals.files else {
            return Ok(Self::empty(today));
        };
        let Some(state) = read(&format!("scan.{}.json", files.scan))
            .and_then(|b| serde_json::from_slice::<ScanState>(&b).ok())
            .filter(|s| s.v == 1)
        else {
            return Ok(Self::empty(today));
        };
        let seen = File::open(dir.join(format!("seen.{}.bin", files.seen)))
            .and_then(Seen::read)
            .unwrap_or_default();
        let mut out = Self {
            totals: Totals {
                tracked_from: Day::parse(&totals.tracked_from).unwrap_or(today),
                logs_from: totals.logs_from.as_deref().and_then(Day::parse),
                repos: totals.repos,
                ..Totals::default()
            },
            state,
            seen,
            files: Some(files),
        };
        for (day, sums) in totals.days {
            let Some(day) = Day::parse(&day) else {
                continue;
            };
            for (key, sum) in sums {
                let mut parts = key.splitn(4, '|');
                let (Some(harness), Some(account), Some(model), Some(repo)) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                let harness = match harness {
                    "claude" => Harness::Claude,
                    "codex" => Harness::Codex,
                    _ => continue,
                };
                let t = &mut out.totals;
                let key = Key {
                    day,
                    harness,
                    account: t.intern(account),
                    model: t.intern(model),
                    repo: t.intern(repo),
                };
                t.sums.insert(key, sum);
            }
        }
        Ok(out)
    }

    /// One rename commits a save: the offsets and the seen set go to new
    /// names first, and `totals.json`, which names them, goes last. A scan
    /// stopped anywhere leaves the last save whole, so a log is never counted
    /// twice or skipped.
    pub fn save(&mut self, dir: &Path, today_utc: u16) -> io::Result<()> {
        let put = |name: &str,
                   write: &dyn Fn(&mut io::BufWriter<File>) -> io::Result<()>|
         -> io::Result<()> {
            let tmp = dir.join(format!("{name}.tmp"));
            let file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            let mut out = io::BufWriter::new(file);
            write(&mut out)?;
            out.flush()?;
            fs::rename(tmp, dir.join(name))
        };
        let prev = self.files;
        let scan = prev.map_or(0, |p| p.scan) + 1;
        let seen = match prev {
            Some(p) if self.seen.added == 0 => p.seen,
            _ => scan,
        };
        if seen == scan {
            put(&format!("seen.{scan}.bin"), &|out| {
                self.seen.write(out, today_utc, SEEN_DAYS)
            })?;
        }
        put(&format!("scan.{scan}.json"), &|out| {
            serde_json::to_writer(out, &self.state).map_err(io::Error::other)
        })?;
        let t = &self.totals;
        let mut days: std::collections::BTreeMap<String, HashMap<String, Sum>> = Default::default();
        for (key, sum) in &t.sums {
            let name = format!(
                "{}|{}|{}|{}",
                key.harness.name(),
                t.name(key.account),
                t.name(key.model),
                t.name(key.repo)
            );
            days.entry(key.day.to_string())
                .or_default()
                .insert(name, *sum);
        }
        let file = TotalsFile {
            v: 1,
            tracked_from: t.tracked_from.to_string(),
            logs_from: t.logs_from.map(|d| d.to_string()),
            days,
            repos: t.repos.clone(),
            files: Some(Generation { scan, seen }),
        };
        put("totals.json", &|out| {
            serde_json::to_writer(out, &file).map_err(io::Error::other)
        })?;
        self.files = Some(Generation { scan, seen });
        self.seen.added = 0;
        // What this save replaced, and what a stopped one left behind.
        for entry in fs::read_dir(dir)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let stale = [("scan.", scan), ("seen.", seen)]
                .iter()
                .any(|(prefix, n)| {
                    name.strip_prefix(prefix).is_some_and(|rest| {
                        rest.split('.').next().and_then(|g| g.parse().ok()) != Some(*n)
                    })
                });
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}
