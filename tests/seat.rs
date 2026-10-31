//! Every Claude Code session runs in Claude Code's own directory, and a
//! change of account puts another login there, so the sessions already open
//! take it too. Logins are files here: the sandbox's HOME is not the user's,
//! so hats never touches the Keychain in tests.

mod common;

use std::fs;
use std::io::Write;
use std::process::Stdio;

use common::Sandbox;

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// A Claude Code status line repaint of a session in `~/.claude`.
fn repaint_seat(sb: &Sandbox, five_hour: f64) {
    let input = format!(
        r#"{{"session_id":"s","rate_limits":{{"five_hour":{{"used_percentage":{five_hour},"resets_at":4102444800}}}}}}"#
    );
    let mut child = sb
        .cmd(&["statusline"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn a_login_only_the_seat_had_gets_an_account_before_it_leaves() {
    let sb = Sandbox::new();
    // Signed in with plain `claude`, before hats.
    let out = sb
        .cmd(&["login", "default"])
        .env("FAKE_ORG_UUID", "org-mine")
        .output()
        .unwrap();
    common::assert_success(&out, &["login", "default"]);
    sb.signed_in("work", "org-work");
    let list = sb.ok(&["list"]);
    assert!(list.contains("● default"), "{list}");

    sb.ok(&["use", "work"]);
    assert_eq!(sb.seat_token(), "token-org-work");
    // Named after the mail, holding that login and who it is.
    let kept = sb.claude_dir("user");
    let login = read_json(&kept.join(".credentials.json"));
    assert_eq!(login["claudeAiOauth"]["accessToken"], "token-org-mine");
    let identity = read_json(&kept.join(".claude.json"));
    assert_eq!(identity["oauthAccount"]["organizationUuid"], "org-mine");

    let list = sb.ok(&["list"]);
    assert!(
        !list.contains("default"),
        "listed once, by its account: {list}"
    );
    assert!(list.contains("● work"), "{list}");
    sb.ok(&["use", "user"]);
    assert_eq!(sb.seat_token(), "token-org-mine");
}

#[test]
fn only_the_login_changes_hands() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    // Claude Code's own state in the seat: MCP logins and settings.
    let item = sb.home.join(".claude/.credentials.json");
    let mut seat = read_json(&item);
    seat["mcpOAuth"] = serde_json::json!({"server": "kept"});
    fs::write(&item, seat.to_string()).unwrap();
    let config = sb.home.join(".claude.json");
    let mut global = read_json(&config);
    global["projects"] = serde_json::json!({"/src": {"hasTrustDialogAccepted": true}});
    fs::write(&config, global.to_string()).unwrap();

    sb.ok(&["use", "side"]);
    let seat = read_json(&item);
    assert_eq!(seat["claudeAiOauth"]["accessToken"], "token-org-side");
    assert_eq!(seat["mcpOAuth"]["server"], "kept");
    let global = read_json(&config);
    assert_eq!(global["oauthAccount"]["organizationUuid"], "org-side");
    assert_eq!(global["projects"]["/src"]["hasTrustDialogAccepted"], true);
}

#[test]
fn a_login_renewed_in_the_seat_goes_back_renewed() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    // Claude Code renewed the token while `work` sat there.
    let item = sb.home.join(".claude/.credentials.json");
    fs::write(&item, r#"{"claudeAiOauth":{"accessToken":"renewed"}}"#).unwrap();

    sb.ok(&["use", "side"]);
    let parked = read_json(&sb.claude_dir("work").join(".credentials.json"));
    assert_eq!(parked["claudeAiOauth"]["accessToken"], "renewed");
    sb.ok(&["use", "work"]);
    assert_eq!(sb.seat_token(), "renewed");
}

#[test]
fn the_provider_says_when_the_account_in_use_fills() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.signed_in("spare", "org-spare");
    sb.ok(&["use", "work"]);
    sb.provider_says("token-org-work", 50.0, 20.0);
    sb.provider_says("token-org-side", 10.0, 5.0);
    sb.provider_says("token-org-spare", 30.0, 5.0);

    common::assert_success(&sb.poll(), &["poll"]);
    assert_eq!(sb.current(), "work");
    let side = listed(&sb, "side");
    assert_eq!(side["usedPercent"], 10.0, "{side}");

    sb.provider_says("token-org-work", 95.0, 20.0);
    common::assert_success(&sb.poll(), &["poll"]);
    assert_eq!(sb.current(), "side");
    assert_eq!(sb.seat_token(), "token-org-side");
    let last = sb.registry_lines().pop().unwrap();
    assert_eq!(last["reason"], "threshold");
}

#[test]
fn an_expired_login_out_of_the_seat_is_renewed_and_kept() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    let parked = sb.claude_dir("side").join(".credentials.json");
    fs::write(
        &parked,
        r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"refresh-side","expiresAt":1000},"mcpOAuth":{"m":1}}"#,
    )
    .unwrap();
    sb.provider_says("token-org-work", 10.0, 10.0);
    sb.provider_says("renewed", 42.0, 10.0);

    common::assert_success(&sb.poll(), &["poll"]);
    let login = read_json(&parked);
    assert_eq!(login["claudeAiOauth"]["accessToken"], "renewed");
    assert_eq!(login["claudeAiOauth"]["refreshToken"], "refresh-renewed");
    assert_eq!(login["mcpOAuth"]["m"], 1);
    let renewals = fs::read_to_string(sb.home.join("provider/renewals")).unwrap();
    assert!(renewals.contains("refresh-side"), "{renewals}");
    assert_eq!(listed(&sb, "side")["usedPercent"], 42.0);
}

#[test]
fn the_login_in_the_seat_is_claude_codes_to_renew() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.ok(&["use", "work"]);
    let seat = sb.home.join(".claude/.credentials.json");
    let expired =
        r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"refresh-work","expiresAt":1000}}"#;
    fs::write(&seat, expired).unwrap();

    let out = sb.poll();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("work: its token expired"),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(&seat).unwrap(), expired);
    assert!(!sb.home.join("provider/renewals").exists());
}

#[test]
fn a_session_in_the_seat_leaves_the_numbers_to_the_provider() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    // Its last answer may have come from the account that sat there before.
    repaint_seat(&sb, 95.0);
    assert_eq!(sb.current(), "work");
    assert!(listed(&sb, "work")["limits"].is_null());
}

#[test]
fn a_rate_limit_right_after_a_change_is_the_account_that_left() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    sb.ok(&["use", "side"]);
    let mut hook = sb
        .cmd(&["hook", "stop"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"s","hook_event_name":"StopFailure","error":"rate_limit"}"#)
        .unwrap();
    hook.wait().unwrap();
    assert_eq!(sb.current(), "side");
    assert!(listed(&sb, "side")["limits"].is_null());
}

fn listed(sb: &Sandbox, label: &str) -> serde_json::Value {
    let doc: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == label)
        .unwrap()
        .clone()
}

#[test]
fn choosing_the_account_in_use_records_nothing() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.ok(&["use", "work"]);
    sb.ok(&["use", "work"]);
    sb.ok(&["use", "work"]);
    let changes = sb
        .registry_lines()
        .into_iter()
        .filter(|l| l.get("reason").is_some())
        .count();
    assert_eq!(changes, 1);
}

#[test]
fn a_rate_limit_counts_as_full_until_the_window_the_status_line_named_resets() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    let input = r#"{"session_id":"s","rate_limits":{"five_hour":{"used_percentage":100,"resets_at":4102444800}}}"#;
    let mut child = sb
        .cmd(&["statusline"])
        .env("CLAUDE_CONFIG_DIR", sb.claude_dir("work"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait().unwrap();
    let mut hook = sb
        .cmd(&["hook", "stop"])
        .env("CLAUDE_CONFIG_DIR", sb.claude_dir("work"))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"s","hook_event_name":"StopFailure","error":"rate_limit"}"#)
        .unwrap();
    hook.wait().unwrap();
    let limits = read_json(&sb.home.join(".local/state/hats/limits.json"));
    assert_eq!(limits["accounts"][0]["limitedUntil"], 4102444800_i64);
}

#[test]
fn signing_in_again_to_the_account_in_use_survives_the_next_change() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    // The provider rejected the login in the seat, so the user signs in again.
    let item = sb.home.join(".claude/.credentials.json");
    let mut stale = read_json(&item);
    stale["claudeAiOauth"]["accessToken"] = "rejected".into();
    fs::write(&item, stale.to_string()).unwrap();
    let out = sb
        .cmd(&["login", "work"])
        .env("FAKE_ORG_UUID", "org-work")
        .output()
        .unwrap();
    common::assert_success(&out, &["login", "work"]);
    assert_eq!(sb.seat_token(), "token-org-work");

    sb.ok(&["use", "side"]);
    let parked = read_json(&sb.claude_dir("work").join(".credentials.json"));
    assert_eq!(parked["claudeAiOauth"]["accessToken"], "token-org-work");
}

/// `label`'s parked login, expired, so the next poll has to renew it.
fn expire(sb: &Sandbox, label: &str) {
    fs::write(
        sb.claude_dir(label).join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"old-{label}","refreshToken":"refresh-{label}","expiresAt":1000}}}}"#
        ),
    )
    .unwrap();
}

fn renewal_answers(sb: &Sandbox, status: Option<u16>) {
    let path = sb.home.join("provider/renewal-status");
    match status {
        Some(status) => fs::write(path, status.to_string()).unwrap(),
        None => fs::remove_file(path).unwrap(),
    }
}

#[test]
fn a_login_the_provider_refuses_shows_in_list_until_it_works_again() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    sb.provider_says("token-org-work", 10.0, 10.0);
    sb.provider_says("token-org-side", 0.0, 0.0);
    common::assert_success(&sb.poll(), &["poll"]);

    expire(&sb, "side");
    renewal_answers(&sb, Some(400));
    sb.poll();
    let list = sb.ok(&["list"]);
    let row = list.lines().find(|l| l.contains("side")).unwrap();
    assert!(row.contains("login expired: hats login side"), "{list}");
    assert!(!row.contains('%'), "no old numbers on it: {row}");
    assert_eq!(listed(&sb, "side")["loginRefused"], true);
    let state = fs::read_to_string(sb.home.join(".local/state/hats/limits.json")).unwrap();
    assert!(
        !state.contains("refresh-side") && !state.contains("old-side"),
        "{state}"
    );

    // Signed in again: the next reading that works clears it.
    renewal_answers(&sb, None);
    sb.provider_says("renewed", 30.0, 5.0);
    common::assert_success(&sb.poll(), &["poll"]);
    let list = sb.ok(&["list"]);
    assert!(!list.contains("expired"), "{list}");
    assert_eq!(listed(&sb, "side")["loginRefused"], false);
}

#[test]
fn a_provider_that_fails_for_a_while_is_no_expired_login() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    sb.provider_says("token-org-work", 10.0, 10.0);
    sb.provider_says("token-org-side", 0.0, 0.0);
    common::assert_success(&sb.poll(), &["poll"]);

    expire(&sb, "side");
    for status in [429, 500, 503] {
        renewal_answers(&sb, Some(status));
        sb.poll();
        let list = sb.ok(&["list"]);
        assert!(!list.contains("expired"), "{status}: {list}");
        assert_eq!(listed(&sb, "side")["loginRefused"], false);
    }
}

#[test]
fn auto_switching_never_moves_to_an_expired_login() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.signed_in("spare", "org-spare");
    sb.ok(&["use", "work"]);
    sb.provider_says("token-org-work", 50.0, 10.0);
    sb.provider_says("token-org-side", 0.0, 0.0);
    sb.provider_says("token-org-spare", 60.0, 0.0);
    common::assert_success(&sb.poll(), &["poll"]);
    // `side` reads emptiest, but its login is gone.
    expire(&sb, "side");
    renewal_answers(&sb, Some(400));
    sb.poll();

    sb.provider_says("token-org-work", 95.0, 10.0);
    sb.poll();
    assert_eq!(sb.current(), "spare");

    // With `spare` off too, there is nowhere to go.
    sb.ok(&["use", "work"]);
    sb.ok(&["disable", "spare"]);
    sb.poll();
    assert_eq!(sb.current(), "work");
    let auto = sb.ok(&["auto"]);
    assert!(auto.contains("Nothing to move to"), "{auto}");
}
