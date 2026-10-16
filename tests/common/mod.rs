//! A throwaway HOME with fake `claude` and `codex` programs on PATH. The fakes
//! print the account directory they were started with and simulate login and
//! logout by writing or deleting identity files; no real harness, credential
//! or network is involved.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const ACCOUNT_UUID: &str = "00000000-0000-4000-8000-000000000001";
pub const ORG_UUID: &str = "00000000-0000-4000-8000-0000000000aa";

const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir="${CLAUDE_CONFIG_DIR-}"
json="${dir:-$HOME}/.claude.json"
[ -n "$dir" ] || json="$HOME/.claude.json"
case "$1 $2" in
  "auth login") printf '{"oauthAccount":{"accountUuid":"%s","organizationUuid":"%s","emailAddress":"user@example.com"}}' "$FAKE_ACCOUNT_UUID" "$FAKE_ORG_UUID" > "$json"; echo "logged in" ;;
  "auth logout") printf '{}' > "$json"; echo "logged out" ;;
  *) echo "claude CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR-<unset>} args=$*" ;;
esac
"#;

const FAKE_CODEX: &str = r#"#!/bin/sh
echo "codex CODEX_HOME=${CODEX_HOME-<unset>} args=$*"
"#;

pub struct Sandbox {
    _root: tempfile::TempDir,
    pub home: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let bin = root.path().join("bin");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        for (name, script) in [("claude", FAKE_CLAUDE), ("codex", FAKE_CODEX)] {
            let path = bin.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self {
            _root: root,
            home,
            bin,
        }
    }

    /// The tool with a clean environment: nothing from the real HOME leaks in.
    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hats"));
        cmd.args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("FAKE_ACCOUNT_UUID", ACCOUNT_UUID)
            .env("FAKE_ORG_UUID", ORG_UUID);
        cmd
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args).output().unwrap();
        assert_success(&out, args);
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn data(&self) -> PathBuf {
        self.home.join(".local/share/hats")
    }

    pub fn registry(&self) -> PathBuf {
        self.home.join(".local/state/hats/switches.jsonl")
    }

    pub fn registry_lines(&self) -> Vec<serde_json::Value> {
        match fs::read_to_string(self.registry()) {
            Ok(text) => text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Writes the identity Claude Code stores after a login.
    pub fn login_claude(&self, global_config: &Path) {
        fs::write(
            global_config,
            format!(
                r#"{{"numStartups":3,"oauthAccount":{{"accountUuid":"{ACCOUNT_UUID}","organizationUuid":"{ORG_UUID}","emailAddress":"user@example.com"}}}}"#
            ),
        )
        .unwrap();
    }
}

pub fn assert_success(out: &Output, args: &[&str]) {
    assert!(
        out.status.success(),
        "{args:?} failed: {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

impl Sandbox {
    /// Adds a signed-in Claude account; each organization is a distinct
    /// provider account.
    pub fn signed_in(&self, label: &str, org: &str) {
        self.ok(&["add", label]);
        let out = self
            .cmd(&["login", label])
            .env("FAKE_ORG_UUID", org)
            .output()
            .unwrap();
        assert_success(&out, &["login", label]);
    }

    pub fn claude_dir(&self, label: &str) -> PathBuf {
        self.data().join("claude").join(label)
    }

    /// Runs the status line wrapper as Claude Code would for `label`, with the
    /// given 5-hour and weekly use.
    pub fn repaint(&self, label: &str, five_hour: f64, seven_day: f64) -> Output {
        use std::io::Write;
        let input = format!(
            r#"{{"session_id":"s-{label}","rate_limits":{{"five_hour":{{"used_percentage":{five_hour},"resets_at":4102444800}},"seven_day":{{"used_percentage":{seven_day},"resets_at":4102444800}}}}}}"#
        );
        let mut child = self
            .cmd(&["statusline"])
            .env("CLAUDE_CONFIG_DIR", self.claude_dir(label))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    pub fn current(&self) -> String {
        let doc: serde_json::Value = serde_json::from_str(&self.ok(&["list", "--json"])).unwrap();
        doc["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["current"] == true && a["harness"] == "claude")
            .map(|a| a["label"].as_str().unwrap().to_owned())
            .unwrap()
    }
}
