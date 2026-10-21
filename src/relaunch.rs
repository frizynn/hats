//! The command line for `claude --resume <session>`: the user's options kept,
//! the old session and the initial prompt dropped. `None` tells a command
//! line that is not an interactive session, which `hats run` does not keep a
//! parent for.

use std::ffi::OsString;

/// Claude Code arguments that select the conversation or start a new turn;
/// on a relaunch they are replaced by `--resume <session>`.
const DROP_WITH_VALUE: &[&str] = &["--session-id"];
const DROP_WITH_OPTIONAL_VALUE: &[&str] = &["-r", "--resume", "--from-pr", "--teleport"];
const DROP: &[&str] = &["-c", "--continue", "--fork-session"];
/// Kept with their single value (Claude Code 2.1.301 `--help`).
const WITH_VALUE: &[&str] = &[
    "--agent",
    "--agents",
    "--append-system-prompt",
    "--autocompact",
    "--debug-file",
    "--effort",
    "--environment",
    "--fallback-model",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--model",
    "-n",
    "--name",
    "--output-format",
    "--permission-mode",
    "--permission-prompt-tool",
    "--remote-control-session-name-prefix",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-snapshot",
];
/// Kept with every following value that does not start with `-`.
const WITH_VALUES: &[&str] = &[
    "--add-dir",
    "--allowedTools",
    "--allowed-tools",
    "--betas",
    "--disallowedTools",
    "--disallowed-tools",
    "--file",
    "--mcp-config",
    "--plugin-dir",
    "--plugin-url",
    "--tools",
];
/// Kept as they are; never followed by a value.
const FLAGS: &[&str] = &[
    "--allow-dangerously-skip-permissions",
    "--ax-screen-reader",
    "--bare",
    "--brief",
    "--chrome",
    "--dangerously-skip-permissions",
    "--disable-slash-commands",
    "--exclude-dynamic-system-prompt-sections",
    "--ide",
    "--no-chrome",
    "--restricted",
    "--safe-mode",
    "--strict-mcp-config",
    "--verbose",
];
/// Not an interactive session: run as a plain exec.
const NOT_INTERACTIVE: &[&str] = &["-p", "--print", "-h", "--help", "-v", "--version"];

/// The arguments for `claude --resume <session>` that keep the user's
/// options, or `None` when they cannot be read safely (a subcommand, print
/// mode, or an option this table does not know followed by a value).
pub fn args(original: &[OsString], session: &str) -> Option<Vec<OsString>> {
    let mut kept: Vec<OsString> = Vec::new();
    let mut args = original.iter().peekable();
    let is_value = |a: Option<&&OsString>| a.is_some_and(|a| !a.to_string_lossy().starts_with('-'));
    let mut saw_prompt = false;
    while let Some(arg) = args.next() {
        let text = arg.to_str()?;
        let name = text.split_once('=').map_or(text, |(n, _)| n);
        let inline_value = text.contains('=');
        if NOT_INTERACTIVE.contains(&name) {
            return None;
        }
        if DROP.contains(&name) {
            continue;
        }
        if DROP_WITH_VALUE.contains(&name) {
            if !inline_value {
                args.next()?;
            }
            continue;
        }
        if DROP_WITH_OPTIONAL_VALUE.contains(&name) {
            if !inline_value && is_value(args.peek()) {
                args.next();
            }
            continue;
        }
        if WITH_VALUE.contains(&name) {
            kept.push(arg.clone());
            if !inline_value {
                kept.push(args.next()?.clone());
            }
            continue;
        }
        if WITH_VALUES.contains(&name) {
            kept.push(arg.clone());
            while is_value(args.peek()) {
                kept.push(args.next()?.clone());
            }
            continue;
        }
        if FLAGS.contains(&name) {
            kept.push(arg.clone());
            continue;
        }
        if text.starts_with('-') {
            // Unknown option: keep it only if it cannot be swallowing a value.
            if is_value(args.peek()) {
                return None;
            }
            kept.push(arg.clone());
            continue;
        }
        // The first word is either a subcommand (`claude mcp ...`) or the
        // initial prompt, which already ran and must not run again.
        if kept.is_empty() && !saw_prompt && SUBCOMMANDS.contains(&text) {
            return None;
        }
        saw_prompt = true;
    }
    kept.push("--resume".into());
    kept.push(session.into());
    Some(kept)
}

const SUBCOMMANDS: &[&str] = &[
    "agents",
    "auth",
    "config",
    "doctor",
    "gateway",
    "install",
    "mcp",
    "migrate-installer",
    "plugin",
    "plugins",
    "setup-token",
    "update",
    "upgrade",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn relaunch(list: &[&str]) -> Option<Vec<String>> {
        super::args(&args(list), "S")
            .map(|v| v.into_iter().map(|a| a.into_string().unwrap()).collect())
    }

    #[test]
    fn keeps_options_and_resumes_the_session() {
        assert_eq!(relaunch(&[]).unwrap(), ["--resume", "S"]);
        assert_eq!(
            relaunch(&[
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "--add-dir",
                "a",
                "b",
                "--verbose"
            ])
            .unwrap(),
            [
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "--add-dir",
                "a",
                "b",
                "--verbose",
                "--resume",
                "S"
            ]
        );
        assert_eq!(
            relaunch(&["--effort=high", "-c"]).unwrap(),
            ["--effort=high", "--resume", "S"]
        );
    }

    #[test]
    fn drops_the_old_session_and_the_initial_prompt() {
        assert_eq!(
            relaunch(&["--resume", "OLD", "--model", "x"]).unwrap(),
            ["--model", "x", "--resume", "S"]
        );
        assert_eq!(relaunch(&["-r"]).unwrap(), ["--resume", "S"]);
        assert_eq!(
            relaunch(&["--session-id", "ID", "fix the bug"]).unwrap(),
            ["--resume", "S"]
        );
        assert_eq!(
            relaunch(&["--model", "x", "fix the bug"]).unwrap(),
            ["--model", "x", "--resume", "S"]
        );
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert_eq!(relaunch(&["-p", "hi"]), None);
        assert_eq!(relaunch(&["mcp", "list"]), None);
        assert_eq!(relaunch(&["--unknown-option", "value"]), None);
        assert!(relaunch(&["--unknown-flag"]).is_some());
    }
}
