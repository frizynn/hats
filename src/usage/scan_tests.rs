//! The scan against synthetic logs written in each harness's own shape.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::usage::accounts::{Book, SwitchLine, parse_switch_log};
use crate::usage::tally::Sum;

fn utc_day_of(ms: i64) -> Day {
    Day(ms.div_euclid(86_400_000) as i32)
}

/// Three hours behind UTC, all year.
fn minus_three(ms: i64) -> Day {
    utc_day_of(ms - 3 * 3_600_000)
}

fn usage(extra: Value) -> Value {
    let mut u = json!({
        "input_tokens": 10, "cache_creation_input_tokens": 300, "cache_read_input_tokens": 1000,
        "output_tokens": 50, "server_tool_use": {"web_search_requests": 0}, "service_tier": "standard",
        "cache_creation": {"ephemeral_1h_input_tokens": 100, "ephemeral_5m_input_tokens": 200},
    });
    for (k, v) in extra.as_object().unwrap() {
        u[k] = v.clone();
    }
    u
}

/// An assistant row in Claude Code's own key order: the scan reads it in
/// place.
fn assistant(id: &str, req: &str, ts: &str) -> Value {
    json!({
        "parentUuid": null, "isSidechain": false,
        "message": {
            "model": "claude-opus-5-5", "id": id, "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "hello"}], "stop_reason": null, "usage": usage(json!({})),
        },
        "requestId": req, "type": "assistant", "uuid": format!("u-{id}"), "timestamp": ts,
        "cwd": "/work/demo", "sessionId": "s", "version": "2.1.0",
    })
}

fn line(v: &Value) -> String {
    format!("{v}\n")
}

struct Fixture {
    tmp: tempfile::TempDir,
    claude: PathBuf,
    codex: PathBuf,
    project: PathBuf,
    tally: Tally,
    lines: Vec<SwitchLine>,
    swap_dirs: HashSet<String>,
    day_of: fn(i64) -> Day,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        let codex = tmp.path().join(".codex");
        let project = claude.join("projects/-work-demo");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(codex.join("sessions/2026/01/10")).unwrap();
        Self {
            claude,
            codex,
            project,
            tmp,
            tally: Tally::empty(Day::parse("2026-01-10").unwrap()),
            lines: Vec::new(),
            swap_dirs: HashSet::new(),
            day_of: utc_day_of,
        }
    }

    fn home(&self) -> &Path {
        self.tmp.path()
    }

    fn run_with(&mut self, chunk: usize) -> Stats {
        let roots = Roots {
            claude: vec![(
                self.claude.to_string_lossy().into_owned(),
                self.claude.join("projects"),
            )],
            codex: vec![self.codex.clone()],
        };
        let book = Book::new(&self.lines, &self.swap_dirs);
        let home = self.tmp.path().to_path_buf();
        scan(
            &mut self.tally,
            &Input {
                roots: &roots,
                book: &book,
                home: &home,
                day_of: self.day_of,
                chunk,
                threads: 4,
            },
        )
    }

    fn run(&mut self) -> Stats {
        self.run_with(1 << 20)
    }

    /// Sums by harness: requests, input, output, write 5m, write 1h, read,
    /// reasoning.
    fn sum(&self, harness: Harness) -> Sum {
        let mut s = [0; 7];
        for (key, v) in &self.tally.totals.sums {
            if key.harness == harness {
                for (a, b) in s.iter_mut().zip(v) {
                    *a += b;
                }
            }
        }
        s
    }

    /// `(day, account, model, project label)` of every total.
    fn keys(&self) -> Vec<(String, String, String, String)> {
        let t = &self.tally.totals;
        let mut keys: Vec<_> = t
            .sums
            .keys()
            .map(|k| {
                let repo = t.name(k.repo);
                (
                    k.day.to_string(),
                    t.name(k.account).to_owned(),
                    t.name(k.model).to_owned(),
                    t.repos
                        .get(repo)
                        .cloned()
                        .unwrap_or_else(|| repo.to_owned()),
                )
            })
            .collect();
        keys.sort();
        keys
    }

    fn write(&self, path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn append(&self, path: &Path, text: &str) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        std::io::Write::write_all(&mut file, text.as_bytes()).unwrap();
    }
}

#[test]
fn a_response_written_once_per_content_block_counts_once_with_its_cache_split() {
    let mut f = Fixture::new();
    let row = assistant("msg_1", "req_1", "2026-01-10T12:00:00.000Z");
    let mut second = row.clone();
    second["message"]["content"] = json!([{"type": "text", "text": "second block"}]);
    f.write(&f.project.join("s1.jsonl"), &(line(&row) + &line(&second)));
    let r = f.run();
    assert_eq!((r.rows, r.duplicates), (1, 1));
    assert_eq!(f.sum(Harness::Claude), [1, 10, 50, 200, 100, 1000, 0]);
}

#[test]
fn a_message_copied_into_another_transcript_counts_once() {
    let mut f = Fixture::new();
    let row = line(&assistant("msg_1", "a", "2026-01-10T12:00:00.000Z"));
    f.write(&f.project.join("s1.jsonl"), &row);
    f.write(&f.project.join("s1/subagents/agent-1.jsonl"), &row);
    f.write(&f.project.join("s2.jsonl"), &row);
    let r = f.run();
    assert_eq!((r.rows, r.duplicates), (1, 2));
}

#[test]
fn the_seen_set_survives_a_save_so_a_later_copy_still_counts_once() {
    let mut f = Fixture::new();
    let row = line(&assistant("msg_1", "a", "2026-01-10T12:00:00.000Z"));
    f.write(&f.project.join("s1.jsonl"), &row);
    f.run();
    let state = f.home().join("state");
    fs::create_dir_all(&state).unwrap();
    f.tally.save(&state, 20_463).unwrap();
    f.tally = Tally::load(&state, Day::parse("2026-01-10").unwrap()).unwrap();
    f.write(&f.project.join("s2.jsonl"), &row);
    let r = f.run();
    assert_eq!((r.rows, r.duplicates, r.read), (0, 1, 1));
    assert_eq!(f.sum(Harness::Claude)[0], 1);
}

#[test]
fn only_top_level_usage_counts_and_a_nested_object_after_it_does_not_hide_the_row() {
    let mut f = Fixture::new();
    let mut row = assistant("msg_1", "r", "2026-01-10T12:00:00.000Z");
    row["message"]["usage"] = usage(json!({
        "iterations": [{"input_tokens": 7777, "output_tokens": 7777}], "speed": "standard",
    }));
    row["message"]["context_management"] =
        json!({"applied_edits": [{"type": "clear_tool_uses_20250919"}]});
    f.write(&f.project.join("s1.jsonl"), &line(&row));
    let r = f.run();
    assert_eq!((r.rows, r.fallback), (1, 0));
    assert_eq!(f.sum(Harness::Claude)[1..3], [10, 50]);
}

#[test]
fn tool_results_with_a_usage_object_and_synthetic_rows_are_not_billed() {
    let mut f = Fixture::new();
    let tool = json!({
        "type": "user", "message": {"role": "user", "content": "x"},
        "toolUseResult": {"usage": {"input_tokens": 999, "output_tokens": 999}},
        "timestamp": "2026-01-10T12:00:00.000Z",
    });
    let mut synthetic = assistant("msg_2", "r2", "2026-01-10T12:00:01.000Z");
    synthetic["message"]["model"] = json!("<synthetic>");
    f.write(
        &f.project.join("s1.jsonl"),
        &(line(&tool) + &line(&synthetic)),
    );
    assert_eq!(f.run().rows, 0);
}

#[test]
fn a_row_the_fast_path_cannot_read_is_parsed_whole_with_the_same_result() {
    let mut f = Fixture::new();
    let row = assistant("msg_1", "a", "2026-01-10T12:00:00.000Z");
    // Keys in another order: not the shape the fast path knows.
    let reordered = json!({
        "type": "assistant", "timestamp": "2026-01-10T12:00:00.000Z", "requestId": "a",
        "message": {"usage": usage(json!({})), "id": "msg_1", "model": "claude-opus-5-5"},
        "cwd": "/work/demo", "uuid": "x",
    });
    f.write(&f.project.join("s1.jsonl"), &line(&reordered));
    f.write(&f.project.join("s2.jsonl"), &line(&row));
    let r = f.run();
    assert_eq!((r.rows, r.duplicates, r.fallback), (1, 1, 1));
    assert_eq!(f.sum(Harness::Claude), [1, 10, 50, 200, 100, 1000, 0]);
}

#[test]
fn fast_mode_and_haikus_long_prompts_keep_their_own_model_for_pricing() {
    let mut f = Fixture::new();
    let mut fast = assistant("msg_1", "a", "2026-01-10T12:00:00.000Z");
    fast["message"]["usage"] = usage(json!({"speed": "fast"}));
    let mut long = assistant("msg_2", "b", "2026-01-10T12:00:01.000Z");
    long["message"]["model"] = json!("claude-haiku-5-5");
    long["message"]["usage"] = usage(json!({"cache_read_input_tokens": 200_000}));
    f.write(&f.project.join("s1.jsonl"), &(line(&fast) + &line(&long)));
    f.run();
    let models: Vec<String> = f.keys().into_iter().map(|k| k.2).collect();
    assert_eq!(models, ["claude-haiku-5-5+long", "claude-opus-5-5+fast"]);
}

#[test]
fn a_half_written_last_line_is_read_when_finished_and_nothing_twice() {
    let mut f = Fixture::new();
    let file = f.project.join("s1.jsonl");
    let second = line(&assistant("msg_2", "b", "2026-01-10T12:00:01.000Z"));
    let first = line(&assistant("msg_1", "a", "2026-01-10T12:00:00.000Z"));
    f.write(&file, &(first.clone() + &second[..40]));
    let r = f.run();
    assert_eq!((r.rows, r.bytes), (1, (first.len() + 40) as u64));
    f.append(&file, &second[40..]);
    let r = f.run();
    // Only the second line is read: the bytes before it are never read again.
    assert_eq!((r.rows, r.bytes), (1, second.len() as u64));
    assert_eq!(f.run().read, 0);
    assert_eq!(f.sum(Harness::Claude)[0], 2);
}

#[test]
fn a_line_longer_than_the_buffer_is_skipped_through_and_read_back_once_when_it_is_a_row() {
    let mut f = Fixture::new();
    let mut big = assistant("msg_1", "a", "2026-01-10T12:00:00.000Z");
    big["message"]["content"] = json!([{"type": "text", "text": "x".repeat(20_000)}]);
    let pad = json!({"type": "user", "pad": "y".repeat(30_000)});
    let text =
        line(&pad) + &line(&big) + &line(&assistant("msg_2", "b", "2026-01-10T12:00:01.000Z"));
    f.write(&f.project.join("s1.jsonl"), &text);
    let r = f.run_with(4096);
    assert_eq!((r.rows, r.bytes), (2, text.len() as u64));
}

#[test]
fn an_unchanged_directory_is_not_listed_again() {
    let mut f = Fixture::new();
    f.write(
        &f.project.join("s1.jsonl"),
        &line(&assistant("msg_1", "a", "2026-01-10T12:00:00.000Z")),
    );
    f.run();
    let mtime = fs::metadata(&f.project).unwrap().modified().unwrap();
    f.write(
        &f.project.join("s2.jsonl"),
        &line(&assistant("msg_2", "b", "2026-01-10T12:00:00.000Z")),
    );
    // The listing is trusted while the directory's mtime holds.
    fs::File::open(&f.project)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert_eq!(f.run().files, 1);
    fs::File::open(&f.project)
        .unwrap()
        .set_modified(mtime + std::time::Duration::from_secs(1))
        .unwrap();
    assert_eq!(f.run().rows, 1);
}

const HATS_LINES: &str = r#"{"v":1,"at":"2026-01-10T09:00:00.000Z","harness":"claude","account":"aaaaaaaaaaaaaaaa","label":"work","configDir":"CONFIG"}
{"v":1,"at":"2026-01-10T11:00:00.000Z","harness":"claude","account":"bbbbbbbbbbbbbbbb","label":"side","configDir":"CONFIG"}
{"v":1,"at":"2026-01-10T11:30:00.000Z","harness":"claude","account":"cccccccccccccccc","label":"spare","configDir":"/elsewhere","session":"s2"}"#;

#[test]
fn each_message_goes_to_the_account_of_its_session_or_config_dir_when_written() {
    let mut f = Fixture::new();
    f.lines = parse_switch_log(&HATS_LINES.replace("CONFIG", &f.claude.to_string_lossy()));
    f.write(
        &f.project.join("s1.jsonl"),
        &(line(&assistant("msg_1", "a", "2026-01-10T10:00:00.000Z"))
            + &line(&assistant("msg_2", "b", "2026-01-10T12:00:00.000Z"))),
    );
    // Session s2's own line names its account; its subagent runs in it too.
    f.write(
        &f.project.join("s2/subagents/agent-1.jsonl"),
        &line(&assistant("msg_3", "c", "2026-01-10T12:00:00.000Z")),
    );
    f.run();
    let accounts: Vec<String> = f.keys().into_iter().map(|k| k.1).collect();
    assert_eq!(
        accounts,
        [
            "claude:aaaaaaaaaaaaaaaa",
            "claude:bbbbbbbbbbbbbbbb",
            "claude:cccccccccccccccc"
        ]
    );
}

#[test]
fn claude_swaps_switches_name_the_account_of_the_default_dir() {
    let mut f = Fixture::new();
    let dir = f.claude.to_string_lossy().into_owned();
    let as_utc = |t: [i64; 7]| {
        let days = crate::registry::days_from_civil(t[0], t[1] as u32, t[2] as u32);
        Some((days * 86_400 + t[3] * 3600 + t[4] * 60 + t[5]) * 1000 + t[6])
    };
    f.lines = crate::usage::accounts::parse_claude_swap_log(
        "2026-01-10 09:00:00,000 - INFO - Added account 1\n\
         2026-01-10 11:00:00,000 - INFO - Switched from account 1 to 2\n",
        &dir,
        as_utc,
    );
    f.swap_dirs.insert(dir);
    f.write(
        &f.project.join("s1.jsonl"),
        &(line(&assistant("msg_0", "z", "2026-01-10T08:00:00.000Z"))
            + &line(&assistant("msg_1", "a", "2026-01-10T10:00:00.000Z"))
            + &line(&assistant("msg_2", "b", "2026-01-10T12:00:00.000Z"))),
    );
    f.run();
    let accounts: Vec<String> = f.keys().into_iter().map(|k| k.1).collect();
    assert_eq!(
        accounts,
        ["claude:swap-1", "claude:swap-2", "claude:unassigned"]
    );
}

#[test]
fn days_are_cut_in_local_time() {
    let mut f = Fixture::new();
    f.day_of = minus_three;
    f.write(
        &f.project.join("s1.jsonl"),
        &(line(&assistant("msg_1", "a", "2026-02-01T02:59:00.000Z"))
            + &line(&assistant("msg_2", "b", "2026-02-01T03:00:00.000Z"))),
    );
    f.run();
    let days: Vec<String> = f.keys().into_iter().map(|k| k.0).collect();
    assert_eq!(days, ["2026-01-31", "2026-02-01"]);
}

fn meta(account: Option<&str>) -> Value {
    let mut payload = json!({
        "id": "r1", "cwd": "/work/demo",
        "base_instructions": {"text": "say \"cwd\":\"/nope\""},
    });
    if let Some(a) = account {
        payload["creator_account_id"] = json!(a);
    }
    json!({"timestamp": "2026-01-10T12:00:00.000Z", "type": "session_meta", "payload": payload})
}

fn turn(model: &str) -> Value {
    json!({"timestamp": "2026-01-10T12:00:01.000Z", "type": "turn_context",
        "payload": {"cwd": "/work/demo", "model": model}})
}

fn tokens(ts: &str, total: [u64; 4], last: Option<[u64; 4]>) -> Value {
    let u = |t: [u64; 4]| {
        json!({"input_tokens": t[0], "cached_input_tokens": t[1], "output_tokens": t[2],
            "reasoning_output_tokens": t[3], "total_tokens": 0})
    };
    json!({"timestamp": ts, "type": "event_msg", "payload": {
        "type": "token_count",
        "info": {"total_token_usage": u(total), "last_token_usage": last.map(u)},
        "rate_limits": null,
    }})
}

#[test]
fn codex_cumulative_totals_become_differences_and_a_reset_counts_its_own_row() {
    let mut f = Fixture::new();
    let file = f
        .codex
        .join("sessions/2026/01/10/rollout-2026-01-10T12-00-00-r1.jsonl");
    let rows = [
        meta(Some("acct-1")),
        turn("gpt-6.1-sol"),
        tokens("2026-01-10T12:00:02.000Z", [100, 60, 10, 4], None),
        tokens("2026-01-10T12:00:03.000Z", [100, 60, 10, 4], None),
        tokens("2026-01-10T12:00:04.000Z", [250, 160, 30, 9], None),
        tokens(
            "2026-01-10T12:00:05.000Z",
            [20, 0, 5, 0],
            Some([20, 0, 5, 0]),
        ),
    ];
    f.write(&file, &rows.iter().map(line).collect::<String>());
    let r = f.run();
    assert_eq!(r.rows, 3);
    // Input is the uncached part: (100-60) + (150-100) + 20.
    assert_eq!(f.sum(Harness::Codex), [3, 110, 35, 0, 0, 160, 9]);
    let account = format!("codex:{}", crate::account::short_hash("acct-1"));
    assert_eq!(
        f.keys(),
        [(
            "2026-01-10".to_owned(),
            account,
            "gpt-6.1-sol".to_owned(),
            "demo".to_owned()
        )]
    );
}

#[test]
fn an_archived_rollout_is_the_same_rollout_not_new_usage() {
    let mut f = Fixture::new();
    let name = "rollout-2026-01-10T12-00-00-r1.jsonl";
    let live = f.codex.join("sessions/2026/01/10").join(name);
    let text =
        line(&meta(None)) + &line(&tokens("2026-01-10T12:00:02.000Z", [100, 0, 10, 0], None));
    f.write(&live, &text);
    assert_eq!(f.run().rows, 1);
    fs::create_dir_all(f.codex.join("archived_sessions")).unwrap();
    fs::rename(&live, f.codex.join("archived_sessions").join(name)).unwrap();
    assert_eq!(f.run().rows, 0);
    // A copy in both places is one rollout too.
    f.write(&live, &text);
    assert_eq!(f.run().rows, 0);
    assert_eq!(f.sum(Harness::Codex)[0], 1);
    assert_eq!(f.keys()[0].1, "codex:unassigned");
}

#[test]
fn a_save_cut_short_leaves_the_last_whole_one() {
    let mut f = Fixture::new();
    let file = f.project.join("s1.jsonl");
    f.write(
        &file,
        &line(&assistant("msg_1", "a", "2026-01-10T12:00:00.000Z")),
    );
    f.run();
    let state = f.home().join("state");
    fs::create_dir_all(&state).unwrap();
    f.tally.save(&state, 20_463).unwrap();
    // A later scan stopped after writing its offsets, before its totals.
    fs::write(
        state.join("scan.2.json"),
        r#"{"v":1,"files":{},"dirs":{},"cwds":{}}"#,
    )
    .unwrap();
    fs::write(state.join("seen.2.bin"), b"junk").unwrap();
    f.tally = Tally::load(&state, Day::parse("2026-01-10").unwrap()).unwrap();
    f.append(
        &file,
        &line(&assistant("msg_2", "b", "2026-01-10T12:01:00.000Z")),
    );
    let r = f.run();
    assert_eq!((r.rows, r.duplicates), (1, 0));
    f.tally.save(&state, 20_463).unwrap();
    let mut names: Vec<String> = fs::read_dir(&state)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["scan.2.json", "seen.2.bin", "totals.json"]);
    let reloaded = Tally::load(&state, Day::parse("2026-01-10").unwrap()).unwrap();
    assert_eq!(named(&reloaded.totals), named(&f.tally.totals));
    assert_eq!(named(&reloaded.totals)[0].1[0], 2);
}

fn named(t: &Totals) -> Vec<(String, Sum)> {
    let mut out: Vec<_> = t
        .sums
        .iter()
        .map(|(k, v)| {
            let name = [t.name(k.account), t.name(k.model), t.name(k.repo)].join("|");
            (format!("{} {} {name}", k.day, k.harness.name()), *v)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_seen_set_whose_size_does_not_match_its_count_is_not_trusted() {
    let mut f = Fixture::new();
    f.write(
        &f.project.join("s1.jsonl"),
        &line(&assistant("msg_1", "a", "2026-01-10T12:00:00.000Z")),
    );
    f.run();
    let state = f.home().join("state");
    fs::create_dir_all(&state).unwrap();
    f.tally.save(&state, 20_463).unwrap();
    // A count of four billion keys in a file of four bytes.
    fs::write(state.join("seen.1.bin"), u32::MAX.to_le_bytes()).unwrap();
    let tally = Tally::load(&state, Day::parse("2026-01-10").unwrap()).unwrap();
    assert_eq!(tally.seen.count, 0);
    assert_eq!(named(&tally.totals), named(&f.tally.totals));
}

#[test]
fn a_token_count_no_request_could_have_counts_zero() {
    let mut f = Fixture::new();
    let garbage = |id: &str, req: &str| {
        line(&assistant(id, req, "2026-01-10T12:00:00.000Z")).replace(
            r#""input_tokens":10"#,
            r#""input_tokens":99999999999999999999"#,
        )
    };
    f.write(
        &f.project.join("s1.jsonl"),
        &(garbage("msg_1", "a") + &garbage("msg_2", "b")),
    );
    let r = f.run();
    assert_eq!((r.rows, r.fallback), (2, 0));
    assert_eq!(f.sum(Harness::Claude), [2, 0, 100, 400, 200, 2000, 0]);
}
