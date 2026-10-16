//! `hats import claude-swap` takes order, aliases and settings, never tokens.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::Sandbox;

#[test]
fn imports_order_aliases_and_settings_without_reading_tokens() {
    let sb = Sandbox::new();
    let swap = sb.home.join(".claude-swap-backup");
    fs::create_dir_all(swap.join("credentials")).unwrap();
    fs::write(swap.join("credentials/.creds-1.json"), "fake").unwrap();
    // Unreadable: any attempt to read stored tokens would fail the import.
    fs::set_permissions(swap.join("credentials"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(
        swap.join("sequence.json"),
        r#"{"activeAccountNumber":2,"sequence":[3,1,2,4],"accounts":{
            "1":{"email":"one@example.com","uuid":"u1"},
            "2":{"email":"two@example.com","uuid":"u2","alias":"team"},
            "3":{"email":"three@example.com","uuid":"u3","alias":"bad alias!"},
            "4":{"email":"four@example.com","uuid":"u4","disabled":true}}}"#,
    )
    .unwrap();
    fs::write(
        swap.join("settings.json"),
        r#"{"schemaVersion":1,"autoswitch":{"threshold":87.6,"strategy":"best"}}"#,
    )
    .unwrap();

    let out = sb.cmd(&["import", "claude-swap"]).output().unwrap();
    fs::set_permissions(swap.join("credentials"), fs::Permissions::from_mode(0o700)).unwrap();
    common::assert_success(&out, &["import", "claude-swap"]);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        sb.ok(&["config", "get", "claude.order"]).trim(),
        "account-3,account-1,team"
    );
    assert_eq!(sb.ok(&["config", "get", "claude.threshold"]).trim(), "88");
    assert_eq!(
        sb.ok(&["config", "get", "claude.strategy"]).trim(),
        "headroom"
    );
    for label in ["account-3", "account-1", "team"] {
        assert!(sb.data().join("claude").join(label).is_dir(), "{label}");
        assert!(stdout.contains(&format!("hats login {label}")), "{stdout}");
    }
    assert!(
        !sb.data().join("claude/account-4").exists(),
        "disabled accounts are skipped"
    );
    assert!(stdout.contains("disabled in claude-swap"), "{stdout}");
    assert!(stdout.contains("cswap purge"), "{stdout}");
    // Labels, the registry and the config never carry mails.
    let config = fs::read_to_string(sb.home.join(".config/hats/config.toml")).unwrap();
    assert!(!config.contains('@'));
    assert!(sb.registry_lines().is_empty());
}

#[test]
fn says_what_is_missing() {
    let sb = Sandbox::new();
    let out = sb
        .cmd(&["import", "claude-swap", "--from", "/nonexistent"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent/sequence.json"));
}

#[test]
fn the_account_already_signed_in_at_home_stays_default() {
    let sb = Sandbox::new();
    // The login claude-swap last switched into ~/.claude.
    sb.login_claude(&sb.home.join(".claude.json"));
    let swap = sb.home.join(".claude-swap-backup");
    fs::create_dir_all(swap.join("configs")).unwrap();
    fs::create_dir_all(swap.join("credentials")).unwrap();
    fs::set_permissions(swap.join("credentials"), fs::Permissions::from_mode(0o000)).unwrap();
    // activeAccountNumber is stale on purpose: the ids decide, not this field.
    fs::write(
        swap.join("sequence.json"),
        r#"{"activeAccountNumber":1,"sequence":[1,2,3],"accounts":{
            "1":{"email":"one@example.com"},
            "2":{"email":"two@example.com"},
            "3":{"email":"three@example.com"}}}"#,
    )
    .unwrap();
    let copy = |n: u32, email: &str, org: &str| {
        fs::write(
            swap.join(format!("configs/.claude-config-{n}-{email}.json")),
            format!(
                r#"{{"numStartups":9,"oauthAccount":{{"accountUuid":"{}","organizationUuid":"{org}","emailAddress":"{email}"}}}}"#,
                common::ACCOUNT_UUID
            ),
        )
        .unwrap();
    };
    // Same person in another organization: a different account.
    copy(1, "one@example.com", "00000000-0000-4000-8000-0000000000bb");
    copy(2, "two@example.com", common::ORG_UUID);
    // Account 3 has no configuration copy.

    let out = sb.cmd(&["import", "claude-swap"]).output().unwrap();
    fs::set_permissions(swap.join("credentials"), fs::Permissions::from_mode(0o700)).unwrap();
    common::assert_success(&out, &["import", "claude-swap"]);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        sb.ok(&["config", "get", "claude.order"]).trim(),
        "account-1,default,account-3"
    );
    assert!(!sb.data().join("claude/account-2").exists());
    assert!(!stdout.contains("hats login account-2"), "{stdout}");
    assert!(stdout.contains("default"), "{stdout}");
    for label in ["account-1", "account-3"] {
        assert!(stdout.contains(&format!("hats login {label}")), "{stdout}");
    }
}

/// `cache/usage.json` as claude-swap 0.26 writes it: per account number, the
/// last good reading and when it was fetched (seconds). Also the mail, which
/// hats never reads.
fn usage(slots: &[(u32, f64, f64, Option<f64>)]) -> String {
    let accounts: serde_json::Map<String, serde_json::Value> = slots
        .iter()
        .map(|&(n, fetched, five, seven)| {
            let window = |pct: f64, at: &str| serde_json::json!({"pct": pct, "resets_at": at, "clock": "10:00", "countdown": "1h"});
            let good = serde_json::json!({
                // Two ways of writing 2099-01-01T10:00:00Z.
                "five_hour": window(five, "2099-01-01T10:00:00.097241+00:00"),
                "seven_day": seven.map(|p| window(p, "2099-01-01T12:00:00.5+02:00")),
                "scoped": [],
            });
            let slot = serde_json::json!({"email": format!("n{n}@example.com"), "fetchedAt": fetched, "lastGood": good, "consecutiveFailures": 0});
            (n.to_string(), slot)
        })
        .collect();
    serde_json::json!({"schemaVersion": 2, "accounts": accounts}).to_string()
}

#[test]
fn takes_claude_swaps_last_limits_so_a_move_never_goes_to_a_spent_account() {
    let sb = Sandbox::new();
    // claude-swap's account 1 is the one signed in at ~/.claude.
    sb.login_claude(&sb.home.join(".claude.json"));
    let swap = sb.home.join(".claude-swap-backup");
    fs::create_dir_all(swap.join("configs")).unwrap();
    fs::create_dir_all(swap.join("cache")).unwrap();
    fs::write(
        swap.join("sequence.json"),
        r#"{"activeAccountNumber":1,"sequence":[1,2,3,4,5],"accounts":{
            "1":{"email":"n1@example.com"},"2":{"email":"n2@example.com"},
            "3":{"email":"n3@example.com"},"4":{"email":"n4@example.com"},
            "5":{"email":"n5@example.com"}}}"#,
    )
    .unwrap();
    let org = |n: u32| {
        if n == 1 {
            common::ORG_UUID.to_owned()
        } else {
            format!("00000000-0000-4000-8000-00000000000{n}")
        }
    };
    for n in 1..=5 {
        fs::write(
            swap.join(format!("configs/.claude-config-{n}-n{n}@example.com.json")),
            format!(
                r#"{{"oauthAccount":{{"accountUuid":"{}","organizationUuid":"{}"}}}}"#,
                common::ACCOUNT_UUID,
                org(n)
            ),
        )
        .unwrap();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let fetched = now - 60.25;
    // What claude-swap showed: account 1 at 13 %, accounts 2 to 5 spent.
    let mut slots = vec![(1, fetched, 13.0, Some(20.0))];
    slots.extend((2..=5).map(|n| (n, fetched, 100.0, None)));
    fs::write(swap.join("cache/usage.json"), usage(&slots)).unwrap();

    sb.ok(&["import", "claude-swap"]);
    for n in 2..=5 {
        let label = format!("account-{n}");
        let out = sb
            .cmd(&["login", &label])
            .env("FAKE_ORG_UUID", org(n))
            .output()
            .unwrap();
        common::assert_success(&out, &["login", &label]);
    }
    let listed = |label: &str| {
        let doc: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
        doc["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["harness"] == "claude" && a["label"] == label)
            .unwrap()
            .clone()
    };
    let default = listed("default");
    assert_eq!(default["usedPercent"], 20.0, "{default}");
    assert_eq!(
        default["limits"]["observedAt"],
        (fetched * 1000.0) as u64,
        "claude-swap's own time"
    );
    let spent = listed("account-2");
    assert_eq!(spent["usedPercent"], 100.0, "{spent}");
    assert_eq!(spent["limits"]["windows"][0]["resetsAt"], 4_070_944_800_i64);
    let state = fs::read_to_string(sb.home.join(".local/state/hats/limits.json")).unwrap();
    assert!(!state.contains('@'), "no mails in hats' files: {state}");

    // A rate limit error on `default` (sessions that still held another
    // login) finds no account with room: the spent ones are known as spent.
    let mut hook = sb
        .cmd(&["hook", "stop"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut hook.stdin.take().unwrap(),
        br#"{"session_id":"s-1","hook_event_name":"StopFailure","error":"rate_limit"}"#,
    )
    .unwrap();
    assert!(hook.wait().unwrap().success());
    assert_eq!(sb.current(), "default");

    // Running it again only refreshes: newer numbers replace the stored ones,
    // older ones do not.
    slots[1] = (2, now - 10.0, 40.0, None);
    fs::write(swap.join("cache/usage.json"), usage(&slots)).unwrap();
    let again = sb.ok(&["import", "claude-swap"]);
    assert!(!again.contains("Created"), "{again}");
    assert_eq!(listed("account-2")["usedPercent"], 40.0);
    slots[1] = (2, now - 3600.0, 90.0, None);
    fs::write(swap.join("cache/usage.json"), usage(&slots)).unwrap();
    sb.ok(&["import", "claude-swap"]);
    assert_eq!(listed("account-2")["usedPercent"], 40.0);
}
