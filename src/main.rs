mod account;
mod auto;
mod config;
mod hook;
mod import;
mod limits;
mod policy;
mod registry;
mod setup;
mod statusline;
mod store;
mod supervisor;
mod tui;
mod view;

use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::SystemTime;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use account::{Account, DEFAULT_LABEL, Harness};
use config::Config;
use store::Store;

const EXAMPLES: &str = "\
Examples:
  hats add work && hats login work      add an account and sign in with Claude Code's own login
  hats run                              start Claude Code with the current account
  hats run personal -- --resume <id>    start it with another account and pass arguments on
  hats use work                         make 'work' the default for new sessions
  hats --codex add personal             the same commands manage Codex accounts
  hats config set claude.threshold 85   switch at 85 % of the 5-hour or weekly limit
  hats setup                            install the hooks, status line and shell functions
  hats                                  open the interactive view";

/// Switch Claude Code and Codex between accounts, one config directory each.
///
/// Each account is a full CLAUDE_CONFIG_DIR or CODEX_HOME where the harness logs
/// in by itself. hats never reads, stores or moves credentials and never calls
/// the providers: limits come from what the harness already shows.
#[derive(Parser)]
#[command(name = "hats", version, after_help = EXAMPLES, propagate_version = true)]
struct Cli {
    /// Act on Codex accounts instead of Claude Code ones.
    #[arg(long, global = true)]
    codex: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List accounts with their limits; `●` marks the one new sessions start with.
    #[command(visible_alias = "ls")]
    List {
        /// Print JSON (schema `v: 1`) for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Create an account that shares your settings, skills and history.
    Add {
        /// Account label, as `hats list` shows it.
        label: String,
    },
    /// Sign in to an account with the harness's own login.
    Login {
        /// Account label, as `hats list` shows it.
        label: String,
        /// Passed on to `claude auth login` or `codex login`.
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Make an account the default for new sessions.
    Use {
        /// Account label, as `hats list` shows it.
        label: String,
    },
    /// Start the harness with an account (default: the current one).
    ///
    /// In auto mode, an interactive Claude Code session stays under a small
    /// hats parent that moves it to the next account when it passes the
    /// threshold, once the agent is idle.
    Run {
        /// Account label, as `hats list` shows it (default: the current one).
        label: Option<String>,
        /// Passed on to the harness.
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Print the shell line that selects an account, for `eval`.
    Env {
        /// Account label, as `hats list` shows it (default: the current one).
        label: Option<String>,
    },
    /// Sign out with the harness and delete the account's directory.
    #[command(visible_alias = "rm")]
    Remove {
        /// Account label, as `hats list` shows it.
        label: String,
    },
    /// Say whether accounts switch by themselves, and turn it on or off.
    ///
    /// Nothing needs to stay running: Claude Code's hooks and status line do
    /// the switching. To watch it happen, leave `hats` open.
    Auto {
        /// `on` or `off`; without it, only say how it is.
        state: Option<OnOff>,
    },
    /// Show or change settings (`hats config` lists them all).
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// Install the Claude Code hooks, the status line wrapper and the shell
    /// functions, showing every change and keeping a backup.
    Setup {
        /// Show what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Remove what setup added.
        #[arg(long)]
        undo: bool,
    },
    /// Import account order and switching settings from another tool.
    Import {
        #[arg(value_enum)]
        source: ImportSource,
        /// Where that tool keeps its data.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Print shell completions: `hats completions zsh > ~/.zfunc/_hats`.
    Completions { shell: clap_complete::Shell },
    /// Open the interactive view (also `hats` with no command).
    Tui,
    /// Claude Code status line wrapper; `hats setup` installs it.
    Statusline {
        /// Your own status line command, run with the same input.
        #[arg(last = true)]
        command: Vec<OsString>,
    },
    /// Claude Code hooks; `hats setup` installs them.
    #[command(hide = true)]
    Hook { event: HookEvent },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Every key with its value and what it does.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print one value: `hats config get claude.mode`.
    Get { key: String },
    /// Change one value: `hats config set codex.strategy next`.
    Set { key: String, value: String },
    /// Open config.toml in $VISUAL or $EDITOR and check it after.
    Edit,
    /// Print where config.toml lives.
    Path,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum ImportSource {
    ClaudeSwap,
}

#[derive(Clone, Copy, ValueEnum)]
enum HookEvent {
    SessionStart,
    Prompt,
    Stop,
}

enum Failure {
    Error(String),
    Exit(u8),
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Exit(code)) => ExitCode::from(code),
        Err(Failure::Error(msg)) => {
            eprintln!("hats: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Failure> {
    let store = Store::from_env(|k| std::env::var_os(k))?;
    let harness = if cli.codex {
        Harness::Codex
    } else {
        Harness::Claude
    };
    let Some(command) = cli.command else {
        return if io::stdout().is_terminal() {
            tui::run(&store).map_err(Failure::Error)
        } else {
            list(&store, false)
        };
    };
    // Hooks run inside the agent's session: they must not fail it, so they
    // report and exit 0.
    if let Cmd::Hook { event } = command {
        if let Err(e) = run_hook(&store, event) {
            eprintln!("hats hook: {e}");
        }
        return Ok(());
    }
    let config = Config::load(&store.config)?;
    match command {
        Cmd::List { json } => list(&store, json)?,
        Cmd::Add { label } => {
            let account = Account::managed(harness, &label, &store)?;
            if account.dir.exists() {
                return Err(format!(
                    "'{label}' already exists; sign in with `hats{} login {label}`",
                    flag(harness)
                )
                .into());
            }
            account.create(&store)?;
            println!(
                "Created {} account '{label}'. Next: hats{} login {label}",
                harness.name(),
                flag(harness)
            );
        }
        Cmd::Login { label, args } => {
            let account = find(&store, harness, &label)?;
            account.create(&store)?;
            let mut cmd = Command::new(harness.program());
            cmd.args(harness.login_args()).args(args);
            return Err(exec(cmd, &account).into());
        }
        Cmd::Use { label } => {
            let account = find(&store, harness, &label)?;
            let id = account
                .provider_id(&store)?
                .ok_or_else(|| not_logged_in(&account))?;
            auto::choose(&store, &account, &id, SystemTime::now())?;
            println!("New {} sessions use '{label}'.", harness.name());
        }
        Cmd::Run { label, args } => {
            return launch(&store, &config, harness, label.as_deref(), args);
        }
        Cmd::Env { label } => println!(
            "{}",
            shell_line(&chosen(&store, harness, label.as_deref())?)
        ),
        Cmd::Remove { label } => remove(&store, find(&store, harness, &label)?)?,
        Cmd::Auto { state } => auto_command(&store, harness, state)?,
        Cmd::Config { action } => config_command(&store, &config, action)?,
        Cmd::Setup { dry_run, undo } => setup::run(&store, dry_run, undo)?,
        Cmd::Import {
            source: ImportSource::ClaudeSwap,
            from,
        } => {
            let from = from.unwrap_or_else(|| store.home.join(".claude-swap-backup"));
            import::claude_swap(&store, &from)?;
        }
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "hats", &mut io::stdout());
        }
        Cmd::Tui => tui::run(&store)?,
        Cmd::Statusline { command } => statusline_command(&store, &config, command)?,
        Cmd::Hook { .. } => unreachable!("handled above"),
    }
    Ok(())
}

fn run_hook(store: &Store, event: HookEvent) -> Result<(), String> {
    let config_dir = std::env::var_os(Harness::Claude.env_var());
    let stdin = io::stdin().lock();
    let now = SystemTime::now();
    match event {
        HookEvent::SessionStart => hook::session_start(store, config_dir, stdin, now),
        HookEvent::Prompt => hook::prompt(store),
        HookEvent::Stop => {
            let config = Config::load(&store.config)?;
            hook::stop(store, &config.claude, config_dir, stdin, now)
        }
    }
}

fn flag(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "",
        Harness::Codex => " --codex",
    }
}

fn find(store: &Store, harness: Harness, label: &str) -> Result<Account, String> {
    Account::find(harness, label, store).map_err(|e| {
        let known: Vec<String> = Account::all(harness, store)
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !a.is_default || a.dir.exists())
            .map(|a| a.label)
            .collect();
        let known = if known.is_empty() {
            "none".to_owned()
        } else {
            known.join(", ")
        };
        format!(
            "{e}. Known: {known}. Add it with `hats{} add {label}`",
            flag(harness)
        )
    })
}

fn chosen(store: &Store, harness: Harness, label: Option<&str>) -> Result<Account, String> {
    match label {
        Some(label) => find(store, harness, label),
        None => store.current(harness),
    }
}

fn not_logged_in(account: &Account) -> String {
    format!(
        "'{}' is not signed in. Run `hats{} login {}`",
        account.label,
        flag(account.harness),
        account.label
    )
}

/// Replaces this process with the harness, so nothing of hats stays running.
fn exec(mut cmd: Command, account: &Account) -> String {
    account.select(&mut cmd);
    let err = cmd.exec();
    format!("could not start {}: {err}", account.harness.program())
}

fn launch(
    store: &Store,
    config: &Config,
    harness: Harness,
    label: Option<&str>,
    args: Vec<OsString>,
) -> Result<(), Failure> {
    let settings = config.for_harness(harness);
    if label.is_none() {
        // A new session should not start on an account that is already spent.
        auto::check_default(store, harness, settings, SystemTime::now(), None, None)?;
    }
    let account = chosen(store, harness, label)?;
    // Picks up shared files the user created since the account was added.
    account.create(store)?;
    if supervisor::should_supervise(&account, settings, &args) {
        let code = supervisor::supervise(store, settings, account, args)?;
        return Err(Failure::Exit(code.clamp(0, 255) as u8));
    }
    let mut cmd = Command::new(harness.program());
    cmd.args(args);
    Err(exec(cmd, &account).into())
}

fn shell_line(account: &Account) -> String {
    let var = account.harness.env_var();
    if account.is_default {
        format!("unset {var}")
    } else {
        format!(
            "export {var}={}",
            shell_quote(&account.dir.to_string_lossy())
        )
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Columns of the terminal on stdout, else `$COLUMNS`, else 80.
fn width() -> usize {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: TIOCGWINSZ fills a winsize; on failure it is left zeroed.
    let size = unsafe {
        libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr());
        size.assume_init()
    };
    if size.ws_col > 0 {
        return usize::from(size.ws_col);
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .filter(|c| *c > 0)
        .unwrap_or(80)
}

fn list(store: &Store, json: bool) -> Result<(), Failure> {
    let config = Config::load(&store.config)?;
    let now = SystemTime::now();
    let width = width();
    let mut out = io::stdout().lock();
    let mut rows = Vec::new();
    let mut any = false;
    for harness in Harness::ALL {
        let settings = config.for_harness(harness);
        let statuses = auto::statuses(store, harness, settings)?;
        if statuses.is_empty() {
            continue;
        }
        if any && !json {
            writeln!(out)?;
        }
        any = true;
        let current = store.current(harness).ok().map(|a| a.label);
        if json {
            for s in &statuses {
                rows.push(serde_json::json!({
                    "harness": harness,
                    "label": s.account.label,
                    "account": s.id,
                    "signedIn": s.id.is_some(),
                    "current": current.as_deref() == Some(s.account.label.as_str()),
                    "configDir": s.account.dir,
                    "usedPercent": s.used(now),
                    "email": s.identity.email,
                    "plan": s.identity.plan,
                    "organization": s.identity.organization,
                    "limits": s.reading.as_ref().map(|r| {
                        let mut limits = serde_json::json!({
                            "observedAt": r.observed_at,
                            "windows": r.windows,
                        });
                        if let Some(until) = r.limited_until {
                            limits["limitedUntil"] = until.into();
                        }
                        limits
                    }),
                }));
            }
            continue;
        }
        let block = view::block(
            harness,
            settings,
            &statuses,
            current.as_deref(),
            width,
            now,
            view::Hints::Commands,
        );
        for row in block.header.iter().chain(block.accounts.iter().flatten()) {
            writeln!(out, "{}", view::plain(row))?;
        }
    }
    if json {
        let doc = serde_json::json!({ "v": 1, "accounts": rows });
        serde_json::to_writer(&mut out, &doc).map_err(io::Error::other)?;
        writeln!(out)?;
    } else if !any {
        writeln!(out, "No accounts yet. Add one with `hats add <label>`.")?;
    }
    Ok(())
}

fn auto_command(store: &Store, harness: Harness, state: Option<OnOff>) -> Result<(), Failure> {
    if let Some(state) = state {
        let mode = match state {
            OnOff::On => config::Mode::Auto,
            OnOff::Off => config::Mode::Manual,
        };
        config::set(
            &store.config,
            &format!("{}.mode", harness.name()),
            mode.as_str(),
        )?;
    }
    let config = Config::load(&store.config)?;
    let current = store.current(harness)?;
    let last = registry::recent(&store.registry(), usize::MAX)
        .into_iter()
        .rfind(|c| c.harness == harness);
    let now = SystemTime::now();
    let lines = view::auto_report(
        harness,
        config.for_harness(harness),
        &current.label,
        setup::installed(store),
        last.as_ref(),
        now,
    );
    let width = width();
    for line in &lines {
        for part in view::wrap(line, width) {
            println!("{part}");
        }
    }
    Ok(())
}

fn config_command(
    store: &Store,
    config: &Config,
    action: Option<ConfigCmd>,
) -> Result<(), Failure> {
    match action.unwrap_or(ConfigCmd::List { json: false }) {
        ConfigCmd::List { json: true } => {
            let map: serde_json::Map<String, serde_json::Value> = config
                .entries()
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect();
            println!("{}", serde_json::Value::Object(map));
        }
        ConfigCmd::List { json: false } => {
            for (key, value) in config.entries() {
                let short = key.split_once('.').map_or(key.as_str(), |(_, k)| k);
                let help = config::KEYS
                    .iter()
                    .find(|(k, _)| *k == short)
                    .map_or("", |(_, h)| h);
                println!("{key:<17} {value:<10} {help}");
            }
            println!(
                "\nChange one with `hats config set <key> <value>`; file: {}",
                store.config.display()
            );
        }
        ConfigCmd::Get { key } => println!("{}", config.get(&key)?),
        ConfigCmd::Set { key, value } => {
            config::set(&store.config, &key, &value)?;
            println!("{key} = {}", Config::load(&store.config)?.get(&key)?);
        }
        ConfigCmd::Path => println!("{}", store.config.display()),
        ConfigCmd::Edit => {
            config::ensure_file(&store.config)?;
            let editor = std::env::var_os("VISUAL")
                .or_else(|| std::env::var_os("EDITOR"))
                .unwrap_or_else(|| "vi".into());
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{} \"$1\"", editor.to_string_lossy()))
                .arg("hats")
                .arg(&store.config)
                .status()?;
            if !status.success() {
                return Err(format!("the editor exited with {status}").into());
            }
            Config::load(&store.config)
                .map_err(|e| format!("{e}\nFix it with `hats config edit`"))?;
            println!("Saved {}", store.config.display());
        }
    }
    Ok(())
}

fn remove(store: &Store, account: Account) -> Result<(), Failure> {
    if account.is_default {
        return Err(format!(
            "'{DEFAULT_LABEL}' is the harness's own directory; hats never removes it"
        )
        .into());
    }
    if account.provider_id(store)?.is_some() {
        // The harness deletes its own credential; hats never touches it.
        let mut cmd = Command::new(account.harness.program());
        cmd.args(account.harness.logout_args());
        account.select(&mut cmd);
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
        "Removed {} account '{}'.",
        account.harness.name(),
        account.label
    );
    Ok(())
}

fn statusline_command(
    store: &Store,
    config: &Config,
    command: Vec<OsString>,
) -> Result<(), Failure> {
    let mut input = Vec::new();
    io::stdin().lock().take(1 << 20).read_to_end(&mut input)?;
    let config_dir = std::env::var_os(Harness::Claude.env_var());
    let (summary, saved) =
        statusline::record(store, &config.claude, config_dir, &input, SystemTime::now());
    if let Err(e) = saved {
        eprintln!("hats statusline: {e}");
    }
    let Some((program, args)) = command.split_first() else {
        println!("{summary}");
        return Ok(());
    };
    // The user's own command gets the same input; its output and exit status
    // are what Claude Code sees.
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", program.to_string_lossy()))?;
    let mut stdin = child.stdin.take().expect("piped");
    // A command that does not read its input closes the pipe early; not an error.
    let _ = stdin.write_all(&input);
    drop(stdin);
    let status = child.wait()?;
    match status.code() {
        Some(0) => Ok(()),
        code => Err(Failure::Exit(code.unwrap_or(1).clamp(1, 255) as u8)),
    }
}
