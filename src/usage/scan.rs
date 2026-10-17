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

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, mpsc};
use std::time::Instant;

use memchr::{memchr, memmem, memrchr};
use serde::{Deserialize, Serialize};

use super::Day;
use super::accounts::{Book, CODEX_UNASSIGNED, codex_account};
use super::bytes::{
    find, hash, iso_ms_at, object_end, rfind, starts_with_at, string_end, text, top_level_fields,
    uint_at,
};
use super::repos::repo_of;
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
    files: Option<Generation>,
}

impl Totals {
    pub fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.index.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_owned());
        self.index.insert(name.to_owned(), id);
        id
    }

    fn add(&mut self, key: Key, tokens: &[u64; 6]) {
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
    o: u64,
    /// Size at the last scan.
    s: u64,
    /// Project key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    r: Option<String>,
    /// Codex: model, account and cumulative totals so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    m: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    a: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    t: Option<[u64; 5]>,
}

/// A directory's mtime (ns), subdirectories and logs: an unchanged directory
/// is not listed again.
#[derive(Clone, Serialize, Deserialize)]
struct Listing(i64, Vec<String>, Vec<String>);

#[derive(Default, Serialize, Deserialize)]
pub struct ScanState {
    v: u32,
    files: HashMap<String, FileEntry>,
    dirs: HashMap<String, Listing>,
    /// cwd to project key.
    cwds: HashMap<String, String>,
}

/// Claude message keys already counted, so a response written once per
/// content block, or copied into another transcript, counts once. Open
/// addressing over two arrays: 10 bytes a slot instead of a map's entry.
pub struct Seen {
    keys: Vec<u64>,
    /// Days since 1970 (UTC) of each key, to forget old ones when saving.
    days: Vec<u16>,
    count: usize,
    added: usize,
}

impl Default for Seen {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl Seen {
    fn with_capacity(n: usize) -> Self {
        let size = (n * 2).next_power_of_two().max(1024);
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

    /// `[u32 count][u32 0][u64 keys][u16 days]`, without keys older than
    /// `keep` days before `today`.
    fn encode(&self, today: u16, keep: u16) -> Vec<u8> {
        let cutoff = today.saturating_sub(keep);
        let live: Vec<(u64, u16)> = self
            .keys
            .iter()
            .zip(&self.days)
            .filter(|(k, d)| **k != 0 && **d >= cutoff)
            .map(|(k, d)| (*k, *d))
            .collect();
        let mut out = Vec::with_capacity(8 + live.len() * 10);
        out.extend_from_slice(&(live.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        for (k, _) in &live {
            out.extend_from_slice(&k.to_le_bytes());
        }
        for (_, d) in &live {
            out.extend_from_slice(&d.to_le_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Self {
        let n = match bytes.get(..4) {
            Some(n) => u32::from_le_bytes(n.try_into().expect("4 bytes")) as usize,
            None => return Self::default(),
        };
        if bytes.len() != 8 + n * 10 {
            return Self::default();
        }
        let mut seen = Self::with_capacity(n + 1024);
        let (keys, days) = bytes[8..].split_at(n * 8);
        for (k, d) in keys.as_chunks::<8>().0.iter().zip(days.as_chunks::<2>().0) {
            seen.insert(u64::from_le_bytes(*k), u16::from_le_bytes(*d));
        }
        seen
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
    state: ScanState,
    seen: Seen,
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
        }
    }

    /// What the last save left, or an empty tally when there is none: totals
    /// without the offsets they were counted with would count logs twice.
    pub fn load(dir: &Path, today: Day) -> Self {
        let read = |name: &str| fs::read(dir.join(name)).ok();
        let Some(totals) = read("totals.json")
            .and_then(|b| serde_json::from_slice::<TotalsFile>(&b).ok())
            .filter(|t| t.v == 1)
        else {
            return Self::empty(today);
        };
        let Some(files) = totals.files else {
            return Self::empty(today);
        };
        let Some(state) = read(&format!("scan.{}.json", files.scan))
            .and_then(|b| serde_json::from_slice::<ScanState>(&b).ok())
            .filter(|s| s.v == 1)
        else {
            return Self::empty(today);
        };
        let seen = read(&format!("seen.{}.bin", files.seen))
            .map(|b| Seen::decode(&b))
            .unwrap_or_default();
        let mut out = Self {
            totals: Totals {
                tracked_from: Day::parse(&totals.tracked_from).unwrap_or(today),
                logs_from: totals.logs_from.as_deref().and_then(Day::parse),
                repos: totals.repos,
                files: Some(files),
                ..Totals::default()
            },
            state,
            seen,
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
        out
    }

    /// One rename commits a save: the offsets and the seen set go to new
    /// names first, and `totals.json`, which names them, goes last. A scan
    /// stopped anywhere leaves the last save whole, so a log is never counted
    /// twice or skipped.
    pub fn save(&mut self, dir: &Path, today_utc: u16) -> io::Result<()> {
        let put = |name: &str, data: &[u8]| -> io::Result<()> {
            let path = dir.join(name);
            let tmp = dir.join(format!("{name}.tmp"));
            let mut file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(data)?;
            fs::rename(tmp, path)
        };
        let prev = self.totals.files;
        let scan = prev.map_or(0, |p| p.scan) + 1;
        let seen = match prev {
            Some(p) if self.seen.added == 0 => p.seen,
            _ => scan,
        };
        if seen == scan {
            put(
                &format!("seen.{scan}.bin"),
                &self.seen.encode(today_utc, SEEN_DAYS),
            )?;
        }
        put(
            &format!("scan.{scan}.json"),
            &serde_json::to_vec(&self.state).map_err(io::Error::other)?,
        )?;
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
        put(
            "totals.json",
            &serde_json::to_vec(&file).map_err(io::Error::other)?,
        )?;
        self.totals.files = Some(Generation { scan, seen });
        self.seen.added = 0;
        // What this save replaced, and what a stopped one left behind.
        for entry in fs::read_dir(dir)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let keep = |prefix: &str, n: u64| {
                name.strip_prefix(prefix)
                    .and_then(|rest| rest.split('.').next())
                    .is_none_or(|g| g.parse() != Ok(n))
            };
            let stale = if name.starts_with("scan.") {
                keep("scan.", scan)
            } else if name.starts_with("seen.") {
                keep("seen.", seen)
            } else {
                false
            };
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
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

struct Job {
    path: PathBuf,
    key: String,
    size: u64,
    harness: Harness,
    config_dir: String,
    session: Option<String>,
}

/// One request: its key (0 for none), local day, UTC day, context and
/// input, output, cache write 5 min, cache write 1 h, cache read, reasoning.
struct Row {
    key: u64,
    day: Day,
    utc_day: u16,
    ctx: u16,
    tokens: [u64; 6],
}

/// Account, model and project shared by a log's rows.
#[derive(PartialEq)]
struct Ctx {
    account: String,
    model: String,
    repo: String,
}

/// Projects found so far, shared by the reading threads: cwd to project key,
/// and project key to folder name.
struct Repos {
    cwds: HashMap<String, String>,
    labels: HashMap<String, String>,
}

struct Done {
    index: usize,
    harness: Harness,
    entry: Option<FileEntry>,
    ctxs: Vec<Ctx>,
    rows: Vec<Row>,
    bytes: u64,
    fallback: u64,
    skipped: u64,
}

pub fn scan(tally: &mut Tally, input: &Input) -> Stats {
    let started = Instant::now();
    let mut stats = Stats::default();
    let jobs = queue(&mut tally.state, input, &mut stats);
    let repos = Mutex::new(Repos {
        cwds: std::mem::take(&mut tally.state.cwds),
        labels: std::mem::take(&mut tally.totals.repos),
    });
    let next = AtomicUsize::new(0);
    let threads = input.threads.clamp(1, jobs.len().max(1));
    let entries = &tally.state.files;
    let mut read = Vec::new();
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<Done>();
        for _ in 0..threads {
            let tx = tx.clone();
            let (jobs, next, repos) = (&jobs, &next, &repos);
            s.spawn(move || {
                let mut reader = Reader::new(input, repos);
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        break;
                    };
                    let entry = entries.get(&job.key).cloned().unwrap_or_default();
                    if tx.send(reader.read(index, job, entry)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // Counted in log order, whichever thread finishes first.
        let mut pending: Vec<Option<Done>> = (0..jobs.len()).map(|_| None).collect();
        let mut at = 0;
        for done in rx {
            let index = done.index;
            pending[index] = Some(done);
            while let Some(done) = pending.get_mut(at).and_then(Option::take) {
                count(&mut tally.totals, &mut tally.seen, &done, &mut stats);
                if let Some(entry) = done.entry {
                    stats.read += 1;
                    read.push((at, entry));
                }
                at += 1;
            }
        }
    });
    for (at, entry) in read {
        tally.state.files.insert(jobs[at].key.clone(), entry);
    }
    let repos = repos.into_inner().expect("no thread panicked holding it");
    tally.state.cwds = repos.cwds;
    tally.totals.repos = repos.labels;
    stats.ms = started.elapsed().as_millis() as u64;
    stats
}

fn count(totals: &mut Totals, seen: &mut Seen, done: &Done, stats: &mut Stats) {
    stats.bytes += done.bytes;
    stats.fallback += done.fallback;
    stats.skipped += done.skipped;
    let ids: Vec<(u32, u32, u32)> = done
        .ctxs
        .iter()
        .map(|c| {
            (
                totals.intern(&c.account),
                totals.intern(&c.model),
                totals.intern(&c.repo),
            )
        })
        .collect();
    for row in &done.rows {
        if row.key != 0 && !seen.add(row.key, row.utc_day) {
            stats.duplicates += 1;
            continue;
        }
        stats.rows += 1;
        let (account, model, repo) = ids[usize::from(row.ctx)];
        let key = Key {
            day: row.day,
            harness: done.harness,
            account,
            model,
            repo,
        };
        totals.add(key, &row.tokens);
    }
}

/// The logs that grew since the last scan, in walk order, forgetting the
/// directories and logs that no longer exist.
fn queue(state: &mut ScanState, input: &Input, stats: &mut Stats) -> Vec<Job> {
    let mut live_dirs = HashSet::new();
    let mut live_files = HashSet::new();
    let mut jobs = Vec::new();
    let mut push = |state: &ScanState, path: PathBuf, key: String, job: Job| {
        stats.files += 1;
        if !live_files.insert(key.clone()) {
            return;
        }
        let Ok(size) = fs::metadata(&path).map(|m| m.len()) else {
            return;
        };
        if state
            .files
            .get(&key)
            .is_some_and(|e| e.s == size && e.o <= size)
        {
            return;
        }
        jobs.push(Job { size, ..job });
    };
    for (config_dir, projects) in &input.roots.claude {
        let mut logs = Vec::new();
        walk(state, &mut live_dirs, projects, &mut logs, |_| true);
        for path in logs {
            // <project>/<session>.jsonl, or <project>/<session>/subagents/agent-<id>.jsonl
            let rel = path.strip_prefix(projects).unwrap_or(&path);
            let parts: Vec<&std::ffi::OsStr> = rel.iter().collect();
            let session = match parts.as_slice() {
                [_, file] => Path::new(file).file_stem(),
                [_, session, ..] => Some(*session),
                _ => None,
            }
            .map(|s| s.to_string_lossy().into_owned());
            let key = path.to_string_lossy().into_owned();
            let job = Job {
                path: path.clone(),
                key: key.clone(),
                size: 0,
                harness: Harness::Claude,
                config_dir: config_dir.clone(),
                session,
            };
            push(state, path, key, job);
        }
    }
    for home in &input.roots.codex {
        for sub in ["sessions", "archived_sessions"] {
            let mut logs = Vec::new();
            walk(state, &mut live_dirs, &home.join(sub), &mut logs, |n| {
                n.starts_with("rollout-")
            });
            for path in logs {
                let id = path.file_stem().unwrap_or_default().to_string_lossy();
                let key = format!("codex:{id}");
                let job = Job {
                    path: path.clone(),
                    key: key.clone(),
                    size: 0,
                    harness: Harness::Codex,
                    config_dir: home.to_string_lossy().into_owned(),
                    session: None,
                };
                push(state, path, key, job);
            }
        }
    }
    state.dirs.retain(|dir, _| live_dirs.contains(dir));
    state.files.retain(|key, _| live_files.contains(key));
    jobs
}

fn walk(
    state: &mut ScanState,
    live: &mut HashSet<String>,
    dir: &Path,
    out: &mut Vec<PathBuf>,
    keep: fn(&str) -> bool,
) {
    let Some(Listing(_, subdirs, logs)) = list(state, live, dir) else {
        return;
    };
    out.extend(logs.iter().filter(|n| keep(n)).map(|n| dir.join(n)));
    for sub in subdirs {
        walk(state, live, &dir.join(sub), out, keep);
    }
}

/// A directory's listing, reused while its mtime holds.
fn list(state: &mut ScanState, live: &mut HashSet<String>, dir: &Path) -> Option<Listing> {
    let key = dir.to_string_lossy().into_owned();
    let mtime = match fs::metadata(dir) {
        Ok(meta) => meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        Err(_) => return None,
    };
    live.insert(key.clone());
    if let Some(cached) = state.dirs.get(&key).filter(|l| l.0 == mtime) {
        return Some(cached.clone());
    }
    let (mut subdirs, mut logs) = (Vec::new(), Vec::new());
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if kind.is_dir() {
            if name != "tool-results" {
                subdirs.push(name);
            }
        } else if kind.is_file() && name.ends_with(".jsonl") {
            logs.push(name);
        }
    }
    let listing = Listing(mtime, subdirs, logs);
    state.dirs.insert(key, listing.clone());
    Some(listing)
}

const USAGE: &[u8] = br#""usage":{"#;
const T_ASSIST: &[u8] = br#""type":"assistant","uuid""#;
const ASSISTANT: &[u8] = br#""type":"assistant""#;
const MSG_MODEL: &[u8] = br#""message":{"model":""#;
const ID_AFTER_MODEL: &[u8] = br#","id":""#;
const REQUEST_ID: &[u8] = br#","requestId":""#;
const TIMESTAMP: &[u8] = br#""timestamp":""#;
const CWD: &[u8] = br#""cwd":""#;
const USAGE_KEYS: [&[u8]; 6] = [
    b"input_tokens",
    b"output_tokens",
    b"cache_creation_input_tokens",
    b"cache_read_input_tokens",
    b"cache_creation",
    b"speed",
];
const TOKEN_COUNT: &[u8] = br#""token_count""#;
const TURN_CONTEXT: &[u8] = br#""type":"turn_context""#;
const SESSION_META: &[u8] = br#""type":"session_meta""#;
const MODEL: &[u8] = br#""model":""#;
const CREATOR: &[u8] = br#""creator_account_id":""#;

static CLAUDE_MARKERS: LazyLock<[memmem::Finder<'static>; 1]> =
    LazyLock::new(|| [memmem::Finder::new(USAGE)]);
static CODEX_MARKERS: LazyLock<[memmem::Finder<'static>; 3]> =
    LazyLock::new(|| [TOKEN_COUNT, TURN_CONTEXT, SESSION_META].map(memmem::Finder::new));

/// A Claude request as read from its row.
struct Message<'b> {
    key: u64,
    ms: i64,
    model: &'b str,
    fast: bool,
    input: u64,
    output: u64,
    write: u64,
    write_1h: u64,
    read: u64,
}

struct Reader<'a> {
    input: &'a Input<'a>,
    repos: &'a Mutex<Repos>,
    buf: Vec<u8>,
    days: HashMap<i64, Day>,
}

impl<'a> Reader<'a> {
    fn new(input: &'a Input<'a>, repos: &'a Mutex<Repos>) -> Self {
        Self {
            input,
            repos,
            buf: vec![0; input.chunk],
            days: HashMap::new(),
        }
    }

    /// The local day, looked up once per quarter hour: no zone offset is
    /// finer.
    fn day(&mut self, ms: i64) -> Day {
        let quarter = ms.div_euclid(900_000);
        let day_of = self.input.day_of;
        *self
            .days
            .entry(quarter)
            .or_insert_with(|| day_of(quarter * 900_000))
    }

    fn repo(&self, cwd: &str) -> String {
        let mut repos = self.repos.lock().expect("no thread panicked holding it");
        if let Some(key) = repos.cwds.get(cwd) {
            return key.clone();
        }
        let repo = repo_of(cwd, self.input.home);
        repos.cwds.insert(cwd.to_owned(), repo.key.clone());
        repos.labels.insert(repo.key.clone(), repo.label);
        repo.key
    }

    fn read(&mut self, index: usize, job: &Job, mut entry: FileEntry) -> Done {
        let mut out = Out {
            ctxs: Vec::new(),
            rows: Vec::new(),
            fallback: 0,
            skipped: 0,
        };
        let done = |entry, bytes, out: Out| Done {
            index,
            harness: job.harness,
            entry,
            ctxs: out.ctxs,
            rows: out.rows,
            bytes,
            fallback: out.fallback,
            skipped: out.skipped,
        };
        let Ok(file) = File::open(&job.path) else {
            return done(None, 0, out);
        };
        if job.size < entry.o {
            // Rewritten: start over.
            entry.o = 0;
            entry.t = None;
        }
        let markers: &[memmem::Finder] = match job.harness {
            Harness::Claude => &*CLAUDE_MARKERS,
            Harness::Codex => &*CODEX_MARKERS,
        };
        let mut buf = std::mem::take(&mut self.buf);
        let (offset, bytes) =
            read_lines(
                &file,
                &mut buf,
                entry.o,
                job.size,
                markers,
                |line, m| match job.harness {
                    Harness::Claude => self.claude_row(line, m, job, &mut entry, &mut out),
                    Harness::Codex => self.codex_row(line, m, &mut entry, &mut out),
                },
            );
        self.buf = buf;
        entry.o = offset;
        entry.s = job.size;
        done(Some(entry), bytes, out)
    }

    fn claude_row(&mut self, b: &[u8], m: usize, job: &Job, entry: &mut FileEntry, out: &mut Out) {
        let le = b.len();
        let Some(t) = find(b, T_ASSIST, m, le) else {
            // Not the assistant row shape this knows. Tool results carry
            // `usage` too; only an assistant row is billed.
            if find(b, ASSISTANT, 0, le).is_some() {
                self.claude_fallback(b, job, entry, out);
            }
            return;
        };
        let fast = || -> Option<(Message<'_>, Option<&[u8]>)> {
            let mi = find(b, MSG_MODEL, 0, le).filter(|&mi| mi <= m)?;
            let u0 = rfind(b, USAGE, 0, t)?;
            let open = u0 + USAGE.len() - 1;
            let close = object_end(b, open, t)?;
            let model_from = mi + MSG_MODEL.len();
            let model_to = string_end(b, model_from, m)?;
            if !starts_with_at(b, model_to + 1, ID_AFTER_MODEL) {
                return None;
            }
            let id_from = model_to + 1 + ID_AFTER_MODEL.len();
            let id_to = string_end(b, id_from, m)?;
            if !starts_with_at(b, id_from, b"msg_") {
                return None;
            }
            let ri = rfind(b, REQUEST_ID, close + 1, t)?;
            let req_from = ri + REQUEST_ID.len();
            let req_to = string_end(b, req_from, t)?;
            let ts = find(b, TIMESTAMP, t, le)?;
            let ms = iso_ms_at(b, ts + TIMESTAMP.len())?;
            let model = std::str::from_utf8(&b[model_from..model_to]).ok()?;
            let cwd = find(b, CWD, t, le).and_then(|c| {
                let from = c + CWD.len();
                string_end(b, from, le).map(|to| &b[from..to])
            });
            let [input, output, write, read, cache_creation, speed] =
                top_level_fields(b, open, close, &USAGE_KEYS);
            let write_1h = cache_creation
                .filter(|&at| b[at] == b'{')
                .and_then(|at| {
                    let end = object_end(b, at, close + 1)?;
                    let [v] = top_level_fields(b, at, end, &[b"ephemeral_1h_input_tokens"]);
                    v.map(|v| uint_at(b, v))
                })
                .unwrap_or(0);
            let num = |at: Option<usize>| at.map_or(0, |at| uint_at(b, at));
            Some((
                Message {
                    key: hash(&b[id_from..id_to], &b[req_from..req_to]),
                    ms,
                    model,
                    fast: speed.is_some_and(|at| starts_with_at(b, at, br#""fast""#)),
                    input: num(input),
                    output: num(output),
                    write: num(write),
                    write_1h,
                    read: num(read),
                },
                cwd,
            ))
        };
        match fast() {
            Some((msg, cwd)) => {
                if entry.r.is_none()
                    && let Some(cwd) = cwd
                {
                    entry.r = Some(self.repo(&text(cwd)));
                }
                self.claude_count(&msg, job, entry, out);
            }
            None => self.claude_fallback(b, job, entry, out),
        }
    }

    /// A row the fast path cannot vouch for, parsed whole. Same result,
    /// slower.
    fn claude_fallback(&mut self, b: &[u8], job: &Job, entry: &mut FileEntry, out: &mut Out) {
        #[derive(Deserialize)]
        struct Line {
            #[serde(rename = "type")]
            kind: Option<serde_json::Value>,
            cwd: Option<serde_json::Value>,
            #[serde(rename = "requestId")]
            request_id: Option<serde_json::Value>,
            timestamp: Option<serde_json::Value>,
            message: Option<Msg>,
        }
        #[derive(Deserialize)]
        struct Msg {
            id: Option<serde_json::Value>,
            model: Option<serde_json::Value>,
            usage: Option<serde_json::Value>,
        }
        out.fallback += 1;
        let Ok(line) = serde_json::from_slice::<Line>(b) else {
            out.skipped += 1;
            return;
        };
        if entry.r.is_none()
            && let Some(cwd) = line.cwd.as_ref().and_then(|v| v.as_str())
        {
            entry.r = Some(self.repo(cwd));
        }
        let message = line.message.as_ref();
        let usage = message
            .and_then(|m| m.usage.as_ref())
            .filter(|u| u.is_object());
        let ms = line
            .timestamp
            .as_ref()
            .and_then(|v| v.as_str())
            .and_then(|s| iso_ms_at(s.as_bytes(), 0));
        let (Some(usage), Some(ms)) = (usage, ms) else {
            return;
        };
        if line.kind.as_ref().and_then(|v| v.as_str()) != Some("assistant") {
            return;
        }
        let n = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_u64()).unwrap_or(0);
        let id = message.and_then(|m| m.id.as_ref()?.as_str()).unwrap_or("");
        let req = line
            .request_id
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let model = message
            .and_then(|m| m.model.as_ref()?.as_str())
            .unwrap_or("unknown");
        let msg = Message {
            key: if id.is_empty() || req.is_empty() {
                0
            } else {
                hash(id.as_bytes(), req.as_bytes())
            },
            ms,
            model,
            fast: usage.get("speed").and_then(|v| v.as_str()) == Some("fast"),
            input: n(usage.get("input_tokens")),
            output: n(usage.get("output_tokens")),
            write: n(usage.get("cache_creation_input_tokens")),
            write_1h: n(usage
                .get("cache_creation")
                .and_then(|c| c.get("ephemeral_1h_input_tokens"))),
            read: n(usage.get("cache_read_input_tokens")),
        };
        self.claude_count(&msg, job, entry, out);
    }

    fn claude_count(&mut self, msg: &Message, job: &Job, entry: &FileEntry, out: &mut Out) {
        if msg.model == "<synthetic>" {
            return;
        }
        let model = if msg.model.starts_with("claude-haiku-5-5")
            && msg.input + msg.read + msg.write > 100_000
        {
            format!("{}+long", msg.model)
        } else if msg.fast {
            format!("{}+fast", msg.model)
        } else {
            msg.model.to_owned()
        };
        let account = self
            .input
            .book
            .claude(&job.config_dir, job.session.as_deref(), msg.ms);
        let repo = entry.r.as_deref().unwrap_or("?");
        let ctx = out.ctx(account, &model, repo);
        let day = self.day(msg.ms);
        out.rows.push(Row {
            key: msg.key,
            day,
            utc_day: utc_day(msg.ms),
            ctx,
            tokens: [
                msg.input,
                msg.output,
                msg.write.saturating_sub(msg.write_1h),
                msg.write_1h,
                msg.read,
                0,
            ],
        });
    }

    /// Per-session cumulative totals: a row adds the difference from the
    /// previous one.
    fn codex_row(&mut self, b: &[u8], m: usize, entry: &mut FileEntry, out: &mut Out) {
        let le = b.len();
        let string_after = |marker: &[u8], from: usize| {
            let at = find(b, marker, from, le)? + marker.len();
            string_end(b, at, le).map(|end| text(&b[at..end]))
        };
        if starts_with_at(b, m, SESSION_META) {
            entry.a = Some(codex_account(string_after(CREATOR, 0).as_deref()));
            if let Some(cwd) = string_after(CWD, 0) {
                entry.r = Some(self.repo(&cwd));
            }
            return;
        }
        if starts_with_at(b, m, TURN_CONTEXT) {
            if let Some(model) = string_after(MODEL, m) {
                entry.m = Some(model);
            }
            return;
        }
        #[derive(Deserialize)]
        struct Line {
            timestamp: Option<String>,
            payload: Option<Payload>,
        }
        #[derive(Deserialize)]
        struct Payload {
            #[serde(rename = "type")]
            kind: Option<String>,
            info: Option<Info>,
        }
        #[derive(Deserialize)]
        struct Info {
            total_token_usage: Option<Usage>,
            last_token_usage: Option<Usage>,
        }
        #[derive(Deserialize, Default)]
        struct Usage {
            input_tokens: Option<serde_json::Number>,
            cached_input_tokens: Option<serde_json::Number>,
            output_tokens: Option<serde_json::Number>,
            reasoning_output_tokens: Option<serde_json::Number>,
            cache_write_input_tokens: Option<serde_json::Number>,
        }
        impl Usage {
            fn cols(&self) -> [u64; 5] {
                [
                    &self.input_tokens,
                    &self.cached_input_tokens,
                    &self.output_tokens,
                    &self.reasoning_output_tokens,
                    &self.cache_write_input_tokens,
                ]
                .map(|n| n.as_ref().and_then(serde_json::Number::as_u64).unwrap_or(0))
            }
        }
        let Ok(line) = serde_json::from_slice::<Line>(b) else {
            out.skipped += 1;
            return;
        };
        let Some(payload) = line
            .payload
            .filter(|p| p.kind.as_deref() == Some("token_count"))
        else {
            return;
        };
        let Some(ms) = line.timestamp.and_then(|t| iso_ms_at(t.as_bytes(), 0)) else {
            return;
        };
        let Some(info) = payload.info else {
            return;
        };
        let Some(total) = info.total_token_usage else {
            return;
        };
        let now = total.cols();
        let prev = entry.t.unwrap_or_default();
        // A counter that went down was reset (a resumed or compacted
        // session): this row's own use.
        let d = if now.iter().zip(prev).any(|(n, p)| *n < p) {
            info.last_token_usage.unwrap_or_default().cols()
        } else {
            std::array::from_fn(|i| now[i] - prev[i])
        };
        entry.t = Some(now);
        if d.iter().all(|v| *v == 0) {
            return;
        }
        let account = entry.a.as_deref().unwrap_or(CODEX_UNASSIGNED);
        let model = entry.m.as_deref().unwrap_or("unknown");
        let repo = entry.r.as_deref().unwrap_or("?");
        let ctx = out.ctx(account, model, repo);
        let day = self.day(ms);
        // The same columns as Claude: input is the uncached part, cached
        // input is the cache read.
        out.rows.push(Row {
            key: 0,
            day,
            utc_day: utc_day(ms),
            ctx,
            tokens: [d[0].saturating_sub(d[1]), d[2], d[4], 0, d[1], d[3]],
        });
    }
}

struct Out {
    ctxs: Vec<Ctx>,
    rows: Vec<Row>,
    fallback: u64,
    skipped: u64,
}

impl Out {
    fn ctx(&mut self, account: &str, model: &str, repo: &str) -> u16 {
        let same = |c: &Ctx| c.account == account && c.model == model && c.repo == repo;
        if let Some(i) = self.ctxs.iter().rposition(same) {
            return i as u16;
        }
        self.ctxs.push(Ctx {
            account: account.to_owned(),
            model: model.to_owned(),
            repo: repo.to_owned(),
        });
        (self.ctxs.len() - 1) as u16
    }
}

fn utc_day(ms: i64) -> u16 {
    ms.div_euclid(86_400_000).clamp(0, i64::from(u16::MAX)) as u16
}

/// Reads `[from, size)` of `file` a buffer at a time and calls `handle` with
/// each whole line that holds a marker and where in it the first one starts.
/// Returns the offset after the last whole line, and the bytes read.
fn read_lines(
    file: &File,
    buf: &mut [u8],
    from: u64,
    size: u64,
    markers: &[memmem::Finder],
    mut handle: impl FnMut(&[u8], usize),
) -> (u64, u64) {
    let first = |b: &[u8]| markers.iter().filter_map(|f| f.find(b)).min();
    // Kept from a skipped long line so a marker across two reads is seen.
    let overlap = markers.iter().map(|f| f.needle().len()).max().unwrap_or(1) - 1;
    let mut pos = from;
    let mut have = 0usize;
    let mut bytes = 0u64;
    // A line longer than the buffer: where it starts and whether it holds a
    // marker.
    let mut long: Option<(u64, bool)> = None;
    while pos < size {
        let want = (buf.len() - have).min((size - pos) as usize);
        let n = match file.read_at(&mut buf[have..have + want], pos) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let base = pos - have as u64;
        pos += n as u64;
        have += n;
        bytes += n as u64;
        let mut start = 0;
        if let Some((line_start, marked)) = long {
            let nl = memchr(b'\n', &buf[..have]);
            let marked = marked || first(&buf[..nl.unwrap_or(have)]).is_some();
            let Some(nl) = nl else {
                let keep = overlap.min(have);
                buf.copy_within(have - keep..have, 0);
                have = keep;
                long = Some((line_start, marked));
                continue;
            };
            if marked {
                let mut line = vec![0; (base + nl as u64 - line_start) as usize];
                if file.read_exact_at(&mut line, line_start).is_ok()
                    && let Some(m) = first(&line)
                {
                    handle(&line, m);
                }
            }
            long = None;
            start = nl + 1;
        }
        let end = memrchr(b'\n', &buf[start..have]).map_or(start, |i| start + i + 1);
        let mut from = start;
        while from < end {
            let Some(m) = first(&buf[from..end]).map(|m| from + m) else {
                break;
            };
            let ls = memrchr(b'\n', &buf[from..m]).map_or(from, |i| from + i + 1);
            let le = m + memchr(b'\n', &buf[m..end]).expect("end follows a newline");
            handle(&buf[ls..le], m - ls);
            from = le + 1;
        }
        if have - end == buf.len() {
            // One line fills the buffer: skip through it, remembering whether
            // it held a marker.
            long = Some((base + end as u64, first(&buf[..have]).is_some()));
            let keep = overlap.min(have);
            buf.copy_within(have - keep..have, 0);
            have = keep;
        } else {
            buf.copy_within(end..have, 0);
            have -= end;
        }
    }
    let offset = match long {
        Some((line_start, _)) => line_start,
        None => pos - have as u64,
    };
    (offset, bytes)
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
