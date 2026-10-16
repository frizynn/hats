//! Fixes the registry format other tools read. `tests/fixtures/switches.jsonl`
//! is the reference: a change here is a breaking change for those readers.

mod common;

use std::fs;

use common::Sandbox;

/// Field names in written order, e.g. `["v", "at", "harness", ...]`.
fn keys(line: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("\":") {
        let open = rest[..start].rfind('"').unwrap();
        keys.push(rest[open + 1..start].to_owned());
        rest = &rest[start + 2..];
    }
    keys
}

fn is_utc_millis(at: &str) -> bool {
    let b = at.as_bytes();
    b.len() == 24
        && at.ends_with('Z')
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'.',
            23 => *c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

const SESSION_KEYS: [&str; 7] = [
    "v",
    "at",
    "harness",
    "account",
    "label",
    "configDir",
    "session",
];
const DEFAULT_KEYS: [&str; 6] = ["v", "at", "harness", "account", "label", "configDir"];

#[test]
fn fixture_is_the_documented_format() {
    let fixture = include_str!("fixtures/switches.jsonl");
    for line in fixture.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["v"], 1);
        assert!(is_utc_millis(value["at"].as_str().unwrap()), "{line}");
        assert!(matches!(
            value["harness"].as_str(),
            Some("claude" | "codex")
        ));
        let account = value["account"].as_str().unwrap();
        assert_eq!(account.len(), 16);
        assert!(
            account
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        let k = keys(line);
        assert!(k == SESSION_KEYS || k == DEFAULT_KEYS, "{line}");
    }
}

#[test]
fn session_hook_writes_one_line_in_the_documented_format() {
    let sb = Sandbox::new();
    sb.login_claude(&sb.home.join(".claude.json"));

    let mut child = sb
        .cmd(&["hook", "session-start"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"s-1","hook_event_name":"SessionStart","source":"startup","cwd":"/tmp"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "SessionStart stdout reaches the model"
    );

    let text = fs::read_to_string(sb.registry()).unwrap();
    let line = text.strip_suffix('\n').unwrap();
    assert!(!line.contains('\n'));
    assert_eq!(keys(line), SESSION_KEYS);
    let at = &line[line.find("\"at\":\"").unwrap() + 6..][..24];
    assert!(is_utc_millis(at), "{at}");
    let expected = format!(
        r#"{{"v":1,"at":"{at}","harness":"claude","account":"9ae25bfbd05c9e76","label":"default","configDir":"{}","session":"s-1"}}"#,
        sb.home.join(".claude").display()
    );
    assert_eq!(line, expected);
    assert_eq!(common::mode(&sb.registry()), 0o600);
}

#[test]
fn use_writes_a_default_line_without_session() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    sb.ok(&["login", "work"]);
    sb.ok(&["use", "work"]);

    let text = fs::read_to_string(sb.registry()).unwrap();
    let line = text.trim_end();
    assert_eq!(keys(line), DEFAULT_KEYS);
    let value: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(value["label"], "work");
    assert_eq!(value["account"], "9ae25bfbd05c9e76");
    assert_eq!(
        value["configDir"],
        sb.data().join("claude/work").display().to_string()
    );
}
