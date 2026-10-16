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
