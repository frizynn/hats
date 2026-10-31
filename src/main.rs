mod account;
mod auto;
mod background;
mod config;
mod hook;
mod import;
mod limits;
mod policy;
mod poll;
mod process;
mod registry;
mod relaunch;
mod seat;
mod secret;
mod setup;
mod statusline;
mod store;
mod supervisor;
mod tui;
mod usage;
mod view;
mod watch;

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
  hats run personal -- --resume <id>    switch to another account and pass arguments on
  hats use work                         every Claude Code session, open ones too, uses 'work'
  hats --codex add personal             the same commands manage Codex accounts
  hats config set claude.threshold 85   switch at 85 % of the 5-hour or weekly limit
  hats setup                            install the hooks, status line and shell functions
  hats                                  open the interactive view";

/// Switch Claude Code and Codex between accounts, one config directory each.
///
/// Each account is a full CLAUDE_CONFIG_DIR or CODEX_HOME where the harness logs
/// in by itself. Claude Code sessions all run in ~/.claude, and a change of
/// account moves that account's login there, so open sessions follow; limits
/// come from the provider's usage endpoint and what Claude Code shows.
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
    /// List accounts with their limits; `●` marks the one in use.
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
    /// Use an account: every Claude Code session, the open ones too; new
    /// Codex sessions.
    Use {
        /// Account label, as `hats list` shows it.
        label: String,
    },
    /// Let auto switching move to an account again.
    Enable {
        /// Account label, as `hats list` shows it.
        label: String,
    },
    /// Keep auto switching from moving to an account; `hats use` still can.
    Disable {
        /// Account label, as `hats list` shows it.
        label: String,
    },
    /// Start the harness with an account (default: the current one).
    ///
    /// Claude Code always starts in ~/.claude; a label first moves that
    /// account's login there, for the open sessions too. An interactive
    /// session stays under a small hats parent that closes it when unused for
    /// `claude.close_idle` (32h by default).
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
    /// Import accounts, their logins and switching settings from another tool.
    Import {
        #[arg(value_enum)]
        source: ImportSource,
        /// Where that tool keeps its data.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Take its login even for an account already signed in here.
        #[arg(long)]
        replace_logins: bool,
    },
    /// Print shell completions: `hats completions zsh > ~/.zfunc/_hats`.
    Completions { shell: clap_complete::Shell },
    /// Tokens used per day, account, model and project, from the agents' own
    /// logs, and what they would cost at API list price (not what a plan
    /// charges).
    Usage {
        /// Print JSON (schema `v: 1`) with one row per day, account, model
        /// and project.
        #[arg(long)]
        json: bool,
    },
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
    /// Ask the provider for every Claude Code account's limits now; the
    /// status line runs it at most once a minute.
    #[command(hide = true)]
    Poll,
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
    /// `StopFailure` with `asyncRewake`: wakes a cut session on a new account.
    Revive,
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
        match run_hook(&store, event) {
            // Exit code 2 wakes the session (`asyncRewake`); the words on
            // stderr are what the agent reads.
            Ok(Some(words)) => {
                eprintln!("{words}");
                return Err(Failure::Exit(2));
            }
            Ok(None) => {}
            Err(e) => eprintln!("hats hook: {e}"),
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
            let mut account = find(&store, harness, &label)?;
            account.create(&store)?;
            // The seat's copy is the one in use; a new login only in the
            // account's directory would be overwritten when the seat parks it.
            if harness == Harness::Claude && seat::current(&store).label == account.label {
                account = seat::seat(&store);
            }
            // The next `hats list` asks the provider, so a login it refused
            // shows as working again.
            poll::ask_soon(&store);
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
            println!("{}", view::chosen(harness, &label));
            if config.for_harness(harness).disabled.contains(&label) {
                eprintln!("hats: {}", off_warning(harness, &label));
            }
        }
        Cmd::Enable { label } => enable(&store, harness, &label, true)?,
        Cmd::Disable { label } => enable(&store, harness, &label, false)?,
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
            replace_logins,
        } => {
            let from = from.unwrap_or_else(|| import::claude_swap_dir(&store));
            import::claude_swap(&store, &from, replace_logins)?;
        }
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "hats", &mut io::stdout());
        }
        Cmd::Usage { json } => usage_command(&store, json)?,
        Cmd::Tui => tui::run(&store)?,
        Cmd::Statusline { command } => statusline_command(&store, &config, command)?,
        Cmd::Poll => {
            for (label, why) in poll::poll(&store, &config.claude, SystemTime::now()) {
                eprintln!("hats: {label}: {why}");
            }
        }
        Cmd::Hook { .. } => unreachable!("handled above"),
    }
    Ok(())
}

/// The words that wake the session, when the hook wakes it.
fn run_hook(store: &Store, event: HookEvent) -> Result<Option<String>, String> {
    let config_dir = std::env::var_os(Harness::Claude.env_var());
    let stdin = io::stdin().lock();
    let now = SystemTime::now();
    match event {
        HookEvent::SessionStart => hook::session_start(store, config_dir, stdin, now)?,
        HookEvent::Prompt => hook::prompt(store, stdin)?,
        HookEvent::Stop => {
            let config = Config::load(&store.config)?;
            hook::stop(store, &config.claude, config_dir, stdin, now)?;
        }
        HookEvent::Revive => return hook::revive(store, config_dir, stdin),
    }
    Ok(None)
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

/// The account a session starts with. Every Claude Code session runs in
/// Claude Code's own directory: a label puts that account's login there,
/// for the sessions already open too.
fn chosen(store: &Store, harness: Harness, label: Option<&str>) -> Result<Account, String> {
    let account = match label {
        Some(label) => find(store, harness, label)?,
        None => store.current(harness)?,
    };
    if harness == Harness::Codex {
        return Ok(account);
    }
    if label.is_some() {
        let id = account
            .provider_id(store)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| not_logged_in(&account))?;
        auto::choose(store, &account, &id, SystemTime::now())?;
        // Picks up shared files the user created since the account was added.
        account.create(store).map_err(|e| e.to_string())?;
    }
    Ok(seat::seat(store))
}

/// What `hats use` and the view say when the chosen account is off.
pub fn off_warning(harness: Harness, label: &str) -> String {
    format!(
        "'{label}' is disabled: auto switching never moves to it, but you chose it. Enable it with `hats{} enable {label}`.",
        flag(harness)
    )
}

fn enable(store: &Store, harness: Harness, label: &str, enabled: bool) -> Result<(), Failure> {
    let account = find(store, harness, label)?;
    config::set_enabled(&store.config, harness, &account.label, enabled)?;
    let config = Config::load(&store.config)?;
    let settings = config.for_harness(harness);
    if enabled {
        println!("'{label}' is enabled: auto switching may move to it.");
        return Ok(());
    }
    println!(
        "'{label}' is disabled: auto switching never moves to it; `hats{} use {label}` still can.",
        flag(harness)
    );
    if let Some((to, _)) = auto::leave_disabled(store, harness, settings, &account.label)? {
        println!("Moved from '{label}' to '{}'.", to.label);
        println!("{}", view::chosen(harness, &to.label));
        return Ok(());
    }
    let current = store.current(harness)?;
    let statuses = auto::statuses(store, harness, settings)?;
    if !auto::has_destination(&statuses, &current.label, settings, SystemTime::now()) {
        println!(
            "Every other {} account is disabled, not signed in or its login expired: auto switching has nowhere to move from '{}'.",
            harness.name(),
            current.label
        );
    }
    Ok(())
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
    if supervisor::should_supervise(&account, config, &args) {
        let code = supervisor::supervise(store, config, account, args)?;
        return Err(Failure::Exit(code.clamp(0, 255) as u8));
    }
    let mut cmd = Command::new(harness.program());
    // Started from inside a supervised session (`claude -p` in the agent's
    // turn), its hooks must not tell that session's parent it is idle.
    cmd.args(args).env_remove(supervisor::PID_ENV);
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
    if poll::due(store, SystemTime::now()) {
        poll::poll(store, &config.claude, SystemTime::now());
    }
    let now = SystemTime::now();
    let width = width();
    let mut out = io::stdout().lock();
    let mut rows = Vec::new();
    let mut any = false;
    let groups = Harness::ALL
        .into_iter()
        .map(|h| Ok((h, auto::statuses(store, h, config.for_harness(h))?)))
        .collect::<Result<Vec<_>, String>>()?;
    let layout = view::Layout::new(
        groups
            .iter()
            .map(|(h, s)| (*h, config.for_harness(*h), s.as_slice())),
        width,
    );
    for (harness, statuses) in groups {
        let settings = config.for_harness(harness);
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
                    "loginRefused": s.login_refused(),
                    "enabled": !settings.disabled.contains(&s.account.label),
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
            &layout,
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

fn usage_command(store: &Store, json: bool) -> Result<(), Failure> {
    if !usage::scanned_before(store) && io::stderr().is_terminal() {
        eprintln!("Reading the agents' logs for the first time; later runs read only what is new.");
    }
    let report = usage::report(store, SystemTime::now())?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    if json {
        #[derive(serde::Serialize)]
        struct Doc<'a> {
            v: u32,
            #[serde(flatten)]
            report: &'a usage::Report,
        }
        let doc = Doc {
            v: 1,
            report: &report,
        };
        serde_json::to_writer(&mut out, &doc).map_err(io::Error::other)?;
        writeln!(out)?;
    } else {
        for line in usage::summary::render(&report, width()) {
            writeln!(out, "{line}")?;
        }
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
    let settings = config.for_harness(harness);
    let current = store.current(harness)?;
    let last = registry::recent(&store.registry(), usize::MAX)
        .into_iter()
        .rfind(|c| c.harness == harness);
    let now = SystemTime::now();
    let statuses = auto::statuses(store, harness, settings)?;
    let lines = view::auto_report(
        harness,
        settings,
        &current.label,
        setup::installed(store),
        last.as_ref(),
        auto::has_destination(&statuses, &current.label, settings, now),
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
                println!("{key:<17} {value:<10} {}", config::help(short));
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
    // Its login sits in ~/.claude: removing the account would leave it there
    // with nowhere to go back to.
    if account.harness == Harness::Claude && store.current(Harness::Claude)?.dir == account.dir {
        return Err(format!(
            "'{}' is in use; choose another first with `hats use <label>`",
            account.label
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

/// `hats poll` on its own, so the status line never waits for the network.
fn poll_in_background() {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(me);
    cmd.arg("poll")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid(2) between fork and exec: out of the session's process
    // group, so it outlives a status line that Claude Code cancels.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

fn statusline_command(
    store: &Store,
    config: &Config,
    command: Vec<OsString>,
) -> Result<(), Failure> {
    let mut input = Vec::new();
    io::stdin().lock().take(1 << 20).read_to_end(&mut input)?;
    let config_dir = std::env::var_os(Harness::Claude.env_var());
    let now = SystemTime::now();
    let (summary, saved) = statusline::record(store, &config.claude, config_dir, &input, now);
    if let Err(e) = saved {
        eprintln!("hats statusline: {e}");
    }
    if poll::due(store, now) {
        poll_in_background();
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
