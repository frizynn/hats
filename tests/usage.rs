//! `hats usage` reads the agents' own logs in a throwaway HOME: synthetic
//! transcripts and rollouts, never a real one.

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use common::Sandbox;
use serde_json::{Value, json};

/// Three hours behind UTC all year, in POSIX form so no zone database is
/// needed.
const ZONE: &str = "ART3";

fn assistant(id: &str, req: &str, ts: &str) -> String {
    let row = json!({
        "parentUuid": null, "isSidechain": false,
        "message": {
            "model": "claude-opus-5-5", "id": id, "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "hello"}], "stop_reason": null,
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 300,
                "cache_read_input_tokens": 1000, "output_tokens": 50,
                "cache_creation": {"ephemeral_1h_input_tokens": 100, "ephemeral_5m_input_tokens": 200}},
        },
        "requestId": req, "type": "assistant", "uuid": format!("u-{id}"), "timestamp": ts,
        "cwd": "/work/demo", "sessionId": "s", "version": "2.1.0",
    });
    format!("{row}\n")
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn account_id(sb: &Sandbox, label: &str) -> String {
    let doc: Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == label)
        .unwrap()["account"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn usage(sb: &Sandbox, args: &[&str], columns: &str) -> String {
    let out = sb
        .cmd(args)
        .env("TZ", ZONE)
        .env("COLUMNS", columns)
        .output()
        .unwrap();
    common::assert_success(&out, args);
    String::from_utf8(out.stdout).unwrap()
}

fn json_usage(sb: &Sandbox) -> Value {
    serde_json::from_str(&usage(sb, &["usage", "--json"], "80")).unwrap()
}

/// `(date, harness, account label, model, requests)` of each row.
fn rows(doc: &Value) -> Vec<(String, String, String, String, u64)> {
    doc["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["date"].as_str().unwrap().to_owned(),
                r["harness"].as_str().unwrap().to_owned(),
                r["accountLabel"].as_str().unwrap().to_owned(),
                r["model"].as_str().unwrap().to_owned(),
                r["requests"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn counts_each_request_once_by_local_day_and_hats_account_then_reads_only_what_is_new() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    let work = account_id(&sb, "work");
    // Session s1 ran on 'work'; s9 has no line and its dir no login.
    write(
        &sb.registry(),
        &format!(
            "{}\n",
            json!({"v": 1, "at": "2026-01-31T00:00:00.000Z", "harness": "claude", "account": work,
                "label": "work", "configDir": sb.claude_dir("work"), "session": "s1"})
        ),
    );
    // Every account's transcripts live in ~/.claude/projects.
    let project = sb.home.join(".claude/projects/-work-demo");
    let late_evening = assistant("msg_1", "a", "2026-02-01T02:00:00.000Z");
    write(
        &project.join("s1.jsonl"),
        &(late_evening.clone() + &assistant("msg_2", "b", "2026-02-01T03:30:00.000Z")),
    );
    write(&project.join("s1/subagents/agent-1.jsonl"), &late_evening);
    write(
        &project.join("s9.jsonl"),
        &assistant("msg_3", "c", "2026-01-31T12:00:00.000Z"),
    );
    let rollout = sb
        .home
        .join(".codex/sessions/2026/02/01/rollout-2026-02-01T10-00-00-r1.jsonl");
    write(
        &rollout,
        &[
            json!({"timestamp": "2026-02-01T10:00:00.000Z", "type": "session_meta",
                "payload": {"id": "r1", "cwd": "/work/demo", "creator_account_id": "acct-1"}}),
            json!({"timestamp": "2026-02-01T10:00:01.000Z", "type": "turn_context",
                "payload": {"cwd": "/work/demo", "model": "gpt-6.1-sol"}}),
            json!({"timestamp": "2026-02-01T10:00:02.000Z", "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": {
                    "input_tokens": 100, "cached_input_tokens": 60, "output_tokens": 10,
                    "reasoning_output_tokens": 4}}}}),
        ]
        .iter()
        .map(|v| format!("{v}\n"))
        .collect::<String>(),
    );

    let doc = json_usage(&sb);
    assert_eq!(doc["v"], 1);
    let claude = |label: &str, date: &str, requests: u64| {
        (
            date.to_owned(),
            "claude".to_owned(),
            label.to_owned(),
            "claude-opus-5-5".to_owned(),
            requests,
        )
    };
    let codex = rows(&doc)
        .into_iter()
        .find(|r| r.1 == "codex")
        .expect("a codex row");
    assert_eq!(
        rows(&doc),
        [
            // 02:00 UTC is still the 31st three hours west.
            claude("unassigned", "2026-01-31", 1),
            claude("work", "2026-01-31", 1),
            claude("work", "2026-02-01", 1),
            codex.clone(),
        ]
    );
    assert_eq!(
        (codex.0.as_str(), codex.3.as_str(), codex.4),
        ("2026-02-01", "gpt-6.1-sol", 1)
    );
    let first = &doc["rows"][1];
    assert_eq!(
        (
            &first["input"],
            &first["cacheWrite5m"],
            &first["cacheWrite1h"]
        ),
        (&json!(10), &json!(200), &json!(100))
    );
    assert_eq!(first["project"], "demo");
    // 10 × $4 + 50 × $20 + 200 × $5 + 100 × $8 + 1000 × $0.2, per million.
    assert!((first["cost"].as_f64().unwrap() - 0.003_04).abs() < 1e-12);
    assert_eq!(doc["scan"]["duplicates"], 1);

    let appended = assistant("msg_4", "d", "2026-02-01T04:00:00.000Z");
    OpenOptions::new()
        .append(true)
        .open(project.join("s1.jsonl"))
        .unwrap()
        .write_all(appended.as_bytes())
        .unwrap();
    let doc = json_usage(&sb);
    assert_eq!(doc["scan"]["read"], 1);
    assert_eq!(doc["scan"]["bytes"], appended.len());
    assert_eq!(rows(&doc)[2], claude("work", "2026-02-01", 2));
}

/// UTC ISO time of `secs` after the epoch.
fn iso(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[test]
fn the_summary_fits_80_columns_and_adds_tokens_from_110() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // The line hats' SessionStart hook writes for a session on 'work'.
    let work = account_id(&sb, "work");
    write(
        &sb.registry(),
        &format!(
            "{}\n",
            json!({"v": 1, "at": iso(now - 120), "harness": "claude", "account": work,
                "label": "work", "configDir": sb.claude_dir("work"), "session": "s1"})
        ),
    );
    write(
        &sb.home.join(".claude/projects/-work-demo/s1.jsonl"),
        &assistant("msg_1", "a", &iso(now - 60)),
    );
    let narrow = usage(&sb, &["usage"], "80");
    assert!(narrow.lines().all(|l| l.chars().count() <= 80), "{narrow}");
    assert!(narrow.starts_with("USAGE · "), "{narrow}");
    assert!(narrow.contains("not what a plan charges"), "{narrow}");
    let total = narrow.lines().find(|l| l.starts_with("Total")).unwrap();
    assert_eq!(total.matches("$0.00").count(), 4, "{narrow}");
    assert!(
        narrow
            .lines()
            .any(|l| l.trim_start().starts_with("claude work"))
    );
    assert!(
        narrow
            .lines()
            .any(|l| l.trim_start().starts_with("Opus 5.5"))
    );
    assert!(narrow.lines().any(|l| l.trim_start().starts_with("demo")));

    let wide = usage(&sb, &["usage"], "120");
    assert!(wide.lines().all(|l| l.chars().count() <= 120), "{wide}");
    let total = wide.lines().find(|l| l.starts_with("Total")).unwrap();
    // 10 + 50 + 300 + 1000 tokens, in each of the four periods.
    assert_eq!(total.matches("1.4K").count(), 4, "{wide}");
}

#[test]
fn totals_saved_by_a_newer_hats_are_left_alone() {
    let sb = Sandbox::new();
    let totals = sb.home.join(".local/state/hats/usage/totals.json");
    // Days older than Claude Code keeps transcripts: only this file has them.
    let newer = r#"{"v":2,"days":{"2025-01-01":{}}}"#;
    write(&totals, newer);
    let out = sb.cmd(&["usage"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("newer hats"), "{stderr}");
    assert_eq!(fs::read_to_string(&totals).unwrap(), newer);
}
