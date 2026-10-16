mod account;
mod hook;
mod registry;
mod statusline;
mod store;

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::SystemTime;

use account::{Account, Harness};
use registry::Switch;
use store::Store;

const USAGE: &str = "\
hats: one config directory per Claude Code or Codex account

Usage: hats <command> [--codex] [args] [-- harness args]

Commands:
  list                  accounts, their ids and the current one
  add <label>           create an account that shares your settings and history
  login <label>         sign in to it with the harness's own login
  use <label>           start new sessions with it by default
  run [label]           start the harness with it (default: the current one)
  env [label]           print the shell line that selects it
  remove <label>        sign out and delete its directory
  hook session-start    Claude Code SessionStart hook (records the session's account)
  statusline [-- cmd]   Claude Code status line: keeps each account's plan limits
                        and runs your own status line command with the same input
  setup                 print the hook and shell snippets to install

Accounts are Claude Code accounts unless --codex is given. The label
'default' is the harness's own directory (~/.claude or ~/.codex).
";

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(msg)) => {
            eprintln!("hats: {msg}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Error(msg)) => {
            eprintln!("hats: {msg}");
            ExitCode::FAILURE
        }
    }
}

enum Failure {
    Usage(String),
    Error(String),
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure::Error(msg)
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::Error(e.to_string())
    }
}

struct Args {
    command: String,
    positional: Vec<String>,
    harness: Harness,
    passthrough: Vec<OsString>,
}

fn parse(argv: Vec<OsString>) -> Result<Args, Failure> {
    let mut words = Vec::new();
    let mut harness = Harness::Claude;
    let mut rest = argv.into_iter();
    let mut passthrough = Vec::new();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            passthrough = rest.collect();
            break;
        }
        let arg = arg
            .into_string()
            .map_err(|a| Failure::Usage(format!("argument is not UTF-8: {a:?}")))?;
        match arg.as_str() {
            "--codex" => harness = Harness::Codex,
            "--claude" => harness = Harness::Claude,
            "-h" | "--help" => words.insert(0, "help".to_owned()),
            "-V" | "--version" => words.insert(0, "version".to_owned()),
            flag if flag.starts_with('-') => {
                return Err(Failure::Usage(format!("unknown option {flag}")));
            }
            _ => words.push(arg),
        }
    }
    let mut words = words.into_iter();
    Ok(Args {
        command: words.next().unwrap_or_else(|| "help".to_owned()),
        positional: words.collect(),
        harness,
        passthrough,
    })
}

fn run(argv: Vec<OsString>) -> Result<(), Failure> {
    let args = parse(argv)?;
    let store = Store::from_env(|k| std::env::var_os(k))?;
    let harness = args.harness;
    let label = |required: bool| -> Result<Option<&str>, Failure> {
        match (args.positional.as_slice(), required) {
            ([], true) => Err(Failure::Usage(format!("{} needs a label", args.command))),
            ([], false) => Ok(None),
            ([label], _) => Ok(Some(label.as_str())),
            _ => Err(Failure::Usage(format!(
                "{} takes one label; pass harness arguments after --",
                args.command
            ))),
        }
    };
    let chosen = |label: Option<&str>| -> Result<Account, Failure> {
        Ok(match label {
            Some(label) => Account::find(harness, label, &store)?,
            None => store.current(harness)?,
        })
    };

    match args.command.as_str() {
        "help" => print!("{USAGE}"),
        "version" => println!("hats {}", env!("CARGO_PKG_VERSION")),
        "list" | "ls" => list(&store)?,
        "add" => {
            let account = Account::managed(harness, label(true)?.unwrap(), &store)?;
            if account.dir.exists() {
                return Err(format!("'{}' already exists", account.label).into());
            }
            account.create(&store)?;
            println!("created {} account '{}'", harness.name(), account.label);
            println!("next: hats login{} {}", codex_flag(harness), account.label);
        }
        "login" => {
            let account = Account::find(harness, label(true)?.unwrap(), &store)?;
            account.create(&store)?;
            return Err(exec_harness(&account, harness.login_args(), &args.passthrough).into());
        }
        "use" => {
            let account = Account::find(harness, label(true)?.unwrap(), &store)?;
            let id = account
                .provider_id(&store)?
                .ok_or_else(|| not_logged_in(&account))?;
            store.set_current(&account)?;
            let line = Switch::new(
                SystemTime::now(),
                harness,
                &id,
                &account.label,
                &account.dir,
                None,
            );
            registry::append(&store.registry(), &line)?;
            println!("new {} sessions use '{}'", harness.name(), account.label);
        }
        "run" => {
            let account = chosen(label(false)?)?;
            // Picks up shared files the user created since the account was added.
            account.create(&store)?;
            return Err(exec_harness(&account, &[], &args.passthrough).into());
        }
        "env" => println!("{}", shell_line(&chosen(label(false)?)?)),
        "remove" | "rm" => remove(
            &store,
            Account::find(harness, label(true)?.unwrap(), &store)?,
        )?,
        "hook" => match args.positional.as_slice() {
            [event] if event == "session-start" => {
                // A hook must never break the session: report and exit 0.
                let config_dir = std::env::var_os(Harness::Claude.env_var());
                if let Err(e) =
                    hook::session_start(&store, config_dir, io::stdin().lock(), SystemTime::now())
                {
                    eprintln!("hats hook: {e}");
                }
            }
            _ => return Err(Failure::Usage("hook takes: session-start".to_owned())),
        },
        "statusline" => {
            if !args.positional.is_empty() {
                return Err(Failure::Usage(
                    "statusline takes no label; pass your status line command after --".to_owned(),
                ));
            }
            let mut input = Vec::new();
            io::stdin().lock().take(1 << 20).read_to_end(&mut input)?;
            let config_dir = std::env::var_os(Harness::Claude.env_var());
            let (summary, saved) =
                statusline::record(&store, config_dir, &input, SystemTime::now());
            if let Err(e) = saved {
                eprintln!("hats statusline: {e}");
            }
            match args.passthrough.split_first() {
                None => println!("{summary}"),
                Some((program, rest)) => chain(program, rest, &input)?,
            }
        }
        "setup" => setup()?,
        other => return Err(Failure::Usage(format!("unknown command '{other}'"))),
    }
    Ok(())
}

fn codex_flag(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "",
        Harness::Codex => " --codex",
    }
}

fn not_logged_in(account: &Account) -> String {
    format!(
        "'{}' is not logged in: run hats login{} {}",
        account.label,
        codex_flag(account.harness),
        account.label
    )
}

/// Replaces this process with the harness, so nothing of ours stays running.
fn exec_harness(account: &Account, args: &[&str], passthrough: &[OsString]) -> String {
    let mut cmd = Command::new(account.harness.program());
    cmd.args(args).args(passthrough);
    select_account(&mut cmd, account);
    let err = cmd.exec();
    format!("could not start {}: {err}", account.harness.program())
}

/// Runs the user's own status line command with the same input and exits with
/// its status; its output goes straight to Claude Code.
fn chain(program: &OsStr, args: &[OsString], input: &[u8]) -> Result<(), Failure> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", program.to_string_lossy()))?;
    let mut stdin = child.stdin.take().expect("piped");
    // A command that does not read its input closes the pipe early; not an error.
    let _ = stdin.write_all(input);
    drop(stdin);
    let status = child.wait()?;
    std::process::exit(status.code().unwrap_or(1));
}

fn select_account(cmd: &mut Command, account: &Account) {
    let var = account.harness.env_var();
    if account.is_default {
        cmd.env_remove(var);
    } else {
        cmd.env(var, &account.dir);
    }
}

fn shell_line(account: &Account) -> String {
    let var = account.harness.env_var();
    if account.is_default {
        format!("unset {var}")
    } else {
        format!("export {var}={}", shell_quote(account.dir.as_os_str()))
    }
}

fn shell_quote(value: &OsStr) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', r"'\''"))
}

fn list(store: &Store) -> Result<(), Failure> {
    let mut out = io::stdout().lock();
    writeln!(out, "HARNESS ACCOUNT          ID                DIR")?;
    for harness in Harness::ALL {
        let current = store.current(harness).ok();
        for account in Account::all(harness, store)? {
            if account.is_default && !account.dir.exists() {
                continue;
            }
            let id = account
                .provider_id(store)
                .unwrap_or(None)
                .unwrap_or_else(|| "(not logged in)".to_owned());
            let marker = if current.as_ref() == Some(&account) {
                "*"
            } else {
                " "
            };
            writeln!(
                out,
                "{:<7} {marker}{:<15} {:<17} {}",
                harness.name(),
                account.label,
                id,
                tilde(&account.dir, store)
            )?;
        }
    }
    Ok(())
}

fn tilde(path: &Path, store: &Store) -> String {
    match path.strip_prefix(&store.home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn remove(store: &Store, account: Account) -> Result<(), Failure> {
    if account.is_default {
        return Err(format!(
            "'{}' is the harness's own directory; it is never removed",
            account.label
        )
        .into());
    }
    if account.provider_id(store)?.is_some() {
        // The harness deletes its own credential; this tool never touches it.
        let mut cmd = Command::new(account.harness.program());
        cmd.args(account.harness.logout_args());
        select_account(&mut cmd, &account);
        let status = cmd
            .status()
            .map_err(|e| format!("could not start {}: {e}", account.harness.program()))?;
        if !status.success() {
            return Err(format!("sign-out failed ({status}); '{}' was kept", account.label).into());
        }
    }
    // remove_dir_all removes symlinks without following them, so the shared
    // settings and history stay in place.
    std::fs::remove_dir_all(&account.dir)?;
    if store.current(account.harness).is_err() {
        store.set_current(&Account::default_for(account.harness, store))?;
    }
    println!(
        "removed {} account '{}'",
        account.harness.name(),
        account.label
    );
    Ok(())
}

fn setup() -> Result<(), Failure> {
    let exe = std::env::current_exe()?;
    let json = |cmd: String| serde_json::to_string(&cmd).expect("string");
    let hook = json(format!("{} hook session-start", exe.display()));
    let status = json(format!(
        "{} statusline -- <your status line command>",
        exe.display()
    ));
    let exe = shell_quote(exe.as_os_str());
    print!(
        "\
Nothing here is applied for you. Paste what you want:

1. Record which account each Claude Code session uses. In ~/.claude/settings.json
   (accounts made with `add` share that file):

  \"hooks\": {{
    \"SessionStart\": [
      {{ \"hooks\": [ {{ \"type\": \"command\", \"command\": {hook} }} ] }}
    ]
  }}

2. Optional: keep each account's plan limits from the status line. In the same file,
   wrap the status line you already have (drop `-- ...` if you have none):

  \"statusLine\": {{ \"type\": \"command\", \"command\": {status} }}

3. Start plain `claude` and `codex` with the current account. In ~/.zshrc or ~/.bashrc:

  claude() {{ {exe} run -- \"$@\"; }}
  codex() {{ {exe} run --codex -- \"$@\"; }}
"
    );
    Ok(())
}
