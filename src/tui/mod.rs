//! Interactive view: four tabs over the accounts of both harnesses, the
//! tokens they used, every setting and what happened. It blocks until a key,
//! a change of hats' state (a status line, a hook, `hats use` elsewhere) or
//! the end of a usage scan, and draws only after one that changed
//! something, so it costs nothing while idle.

mod accounts;
mod activity;
mod settings;
mod theme;
mod usage;

use std::io::{self, Stdout, Write, stdout};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use crate::account::Harness;
use crate::auto::{self, Status};
use crate::config::Config;
use crate::registry::{self, Change};
use crate::store::Store;
use crate::watch::Watch;
use theme::Theme;

/// Changes of account read for the Activity tab and the accounts' details.
const CHANGES: usize = 200;

pub fn run(store: &Store) -> Result<(), String> {
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    install_panic_hook();
    let mut screen =
        Screen::enter().map_err(|e| format!("could not open the interactive view: {e}"))?;
    let mut app = App::new(store, Theme::detect(color));
    let result = event_loop(&mut app, &mut screen);
    drop(screen);
    // Back on the normal screen, say what closed the view: a terminal that
    // sends Esc or Ctrl+C on its own would otherwise close it unseen.
    if let Some(key) = app.closed_by {
        println!("hats: closed with {key}.");
    }
    result.map_err(|e| e.to_string())
}

fn event_loop(app: &mut App, screen: &mut Screen) -> io::Result<()> {
    // Starts crossterm's reader, which also listens for resizes.
    event::poll(Duration::ZERO)?;
    let mut watch = app.watch();
    screen.term.draw(|frame| app.draw(frame))?;
    loop {
        // No timeout: nothing runs until a key, a change of hats' state or
        // the end of a usage scan.
        let mut redraw = false;
        match &mut watch {
            Some(watch) => {
                let wake = watch.wait()?;
                if wake.changed {
                    app.reload();
                    redraw = true;
                }
                if wake.input {
                    // Everything the terminal sent, without blocking.
                    while event::poll(Duration::ZERO)? {
                        match handle(app, screen, event::read()?)? {
                            None => return Ok(()),
                            Some(again) => redraw |= again,
                        }
                    }
                }
            }
            None => match handle(app, screen, event::read()?)? {
                None => return Ok(()),
                Some(again) => redraw = again,
            },
        }
        redraw |= app.usage.collect();
        if redraw {
            screen.term.draw(|frame| app.draw(frame))?;
        }
    }
}

/// Acts on one terminal event: `None` to quit, else whether to redraw.
fn handle(app: &mut App, screen: &mut Screen, event: Event) -> io::Result<Option<bool>> {
    let step = match event {
        Event::Key(key) => app.on_key(key),
        Event::Resize(..) => Step::Redraw,
        _ => Step::Idle,
    };
    Ok(match step {
        Step::Idle => Some(false),
        Step::Redraw => Some(true),
        Step::Quit => None,
        Step::Run(child) => {
            let outcome = screen.run(&child)?;
            app.finish(&child, outcome);
            Some(true)
        }
    })
}

/// What the loop does after a key.
#[derive(Debug, PartialEq)]
enum Step {
    /// Nothing changed: no redraw.
    Idle,
    Redraw,
    Quit,
    /// Run `hats <action> <label>`, then redraw.
    Run(Child),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Accounts,
    Usage,
    Settings,
    Activity,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Accounts, Tab::Usage, Tab::Settings, Tab::Activity];

    fn title(self) -> &'static str {
        match self {
            Tab::Accounts => "Accounts",
            Tab::Usage => "Usage",
            Tab::Settings => "Settings",
            Tab::Activity => "Activity",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).expect("listed")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Add,
    Login,
    Remove,
}

impl Action {
    fn command(self) -> &'static str {
        match self {
            Action::Add => "add",
            Action::Login => "login",
            Action::Remove => "remove",
        }
    }

    /// Login and sign-out talk to the user through the terminal; add only
    /// creates a directory.
    fn interactive(self) -> bool {
        self != Action::Add
    }
}

/// A run of this same binary, so the CLI's checks and messages are the ones
/// the user gets.
#[derive(Debug, PartialEq)]
struct Child {
    action: Action,
    harness: Harness,
    label: String,
}

impl Child {
    fn command(&self) -> io::Result<Command> {
        let mut cmd = Command::new(std::env::current_exe()?);
        if self.harness == Harness::Codex {
            cmd.arg("--codex");
        }
        cmd.arg(self.action.command()).arg(&self.label);
        Ok(cmd)
    }
}

struct Group {
    harness: Harness,
    statuses: Vec<Status>,
    current: Option<String>,
}

struct Message {
    text: String,
    error: bool,
}

struct App<'a> {
    store: &'a Store,
    config: Config,
    /// One per harness, in `Harness::ALL` order.
    groups: [Group; 2],
    tab: Tab,
    accounts: accounts::State,
    settings: settings::State,
    usage: usage::State,
    activity: activity::State,
    /// The newest changes of account, of both harnesses, oldest first.
    changes: Vec<Change>,
    message: Option<Message>,
    /// The key that closed the view.
    closed_by: Option<&'static str>,
    theme: Theme,
}

impl<'a> App<'a> {
    fn new(store: &'a Store, theme: Theme) -> Self {
        let mut app = Self {
            store,
            config: Config::default(),
            groups: Harness::ALL.map(|harness| Group {
                harness,
                statuses: Vec::new(),
                current: None,
            }),
            tab: Tab::Accounts,
            accounts: accounts::State::default(),
            settings: settings::State::default(),
            usage: usage::State::new(),
            activity: activity::State::default(),
            changes: Vec::new(),
            message: None,
            closed_by: None,
            theme,
        };
        app.reload();
        if app.groups[0].statuses.is_empty() && !app.groups[1].statuses.is_empty() {
            app.accounts.focus = 1;
        }
        app.select_current();
        app
    }

    /// Wakes the view when another process changes the state or a usage
    /// scan ends, if the terminal is on stdin, where crossterm reads keys.
    fn watch(&self) -> Option<Watch> {
        // SAFETY: isatty(3) on stdin.
        let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if !tty {
            return None;
        }
        self.store.ensure_state().ok()?;
        Watch::new(
            &self.store.state,
            libc::STDIN_FILENO,
            Some(self.usage.wake_fd().as_raw_fd()),
        )
        .ok()
    }

    /// Reads config, accounts, limits and activity again, keeping the
    /// selected account.
    fn reload(&mut self) {
        let selected = self.selected_status().map(|s| s.account.label.clone());
        let mut error = None;
        match Config::load(&self.store.config) {
            Ok(config) => self.config = config,
            Err(e) => error = Some(e),
        }
        for group in &mut self.groups {
            let settings = self.config.for_harness(group.harness);
            group.statuses =
                auto::statuses(self.store, group.harness, settings).unwrap_or_else(|e| {
                    error = Some(e);
                    Vec::new()
                });
            group.current = self.store.current(group.harness).ok().map(|a| a.label);
        }
        self.changes = registry::recent(&self.store.registry(), CHANGES);
        self.activity.reload(self.store, &self.changes);
        if let Some(e) = error {
            self.fail(e);
        }
        self.select_label(selected.as_deref());
    }

    fn info(&mut self, text: String) {
        self.message = Some(Message { text, error: false });
    }

    fn fail(&mut self, text: String) {
        self.message = Some(Message { text, error: true });
    }

    fn on_key(&mut self, key: KeyEvent) -> Step {
        if key.kind == KeyEventKind::Release {
            return Step::Idle;
        }
        // Raw mode turns Ctrl+C into a key.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.closed_by = Some("Ctrl+C");
            return Step::Quit;
        }
        let cleared = self.message.take().is_some();
        // A prompt or a value being typed takes every key.
        let typing = self.accounts.prompt.is_some() || self.settings.editing();
        let step = match (typing, key.code) {
            (false, KeyCode::Tab) => self.show(Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()]),
            (false, KeyCode::BackTab) => {
                self.show(Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()])
            }
            (false, KeyCode::Char(c @ '1'..='4')) => {
                self.show(Tab::ALL[usize::from(c as u8 - b'1')])
            }
            (false, KeyCode::Char('q')) => Step::Quit,
            (false, KeyCode::Char('r')) => {
                self.reload();
                if self.tab == Tab::Usage {
                    self.usage.start(self.store);
                }
                if self.message.is_none() {
                    self.info("Reloaded.".to_owned());
                }
                Step::Redraw
            }
            _ => match self.tab {
                Tab::Accounts => self.accounts_key(key),
                Tab::Usage => self.usage_key(key),
                Tab::Settings => self.settings_key(key),
                Tab::Activity => self.activity_key(key),
            },
        };
        if step == Step::Quit && self.closed_by.is_none() {
            self.closed_by = Some(match key.code {
                KeyCode::Esc => "Esc",
                _ => "q",
            });
        }
        match step {
            Step::Idle if cleared => Step::Redraw,
            step => step,
        }
    }

    fn show(&mut self, tab: Tab) -> Step {
        if tab == self.tab {
            return Step::Idle;
        }
        self.tab = tab;
        if tab == Tab::Usage {
            // Each visit reads what the logs gained; nothing scans otherwise.
            self.usage.start(self.store);
        }
        Step::Redraw
    }

    // Drawing.

    fn dim(&self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [tabs, rule, body, message, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_tabs(frame, tabs);
        frame.render_widget(
            Paragraph::new("─".repeat(usize::from(rule.width))).style(self.dim()),
            rule,
        );
        let keys = match self.tab {
            Tab::Accounts => self.draw_accounts(frame, body, message),
            Tab::Usage => self.draw_usage(frame, body),
            Tab::Settings => self.draw_settings(frame, body, message),
            Tab::Activity => self.draw_activity(frame, body),
        };
        if !self.accounts.prompt_shown(self.tab) && !self.settings.edit_shown(self.tab) {
            self.draw_message(frame, message);
        }
        frame.render_widget(Paragraph::new(self.keys(&keys)), footer);
    }

    /// `1 Accounts  2 Usage  3 Settings  4 Activity`, the current one
    /// selected, and the keys that work everywhere on the right.
    fn draw_tabs(&self, frame: &mut Frame, area: Rect) {
        let mut spans = Vec::new();
        for tab in Tab::ALL {
            let label = format!(" {} {} ", tab.index() + 1, tab.title());
            spans.push(if tab == self.tab {
                Span::styled(label, self.theme.selected().patch(self.theme.accent()))
            } else {
                Span::styled(label, self.dim())
            });
        }
        let used: usize = spans.iter().map(|s| s.width()).sum();
        let global = "tab next · q quit";
        let room = usize::from(area.width).saturating_sub(used + global.len());
        if room >= 2 {
            spans.push(Span::raw(" ".repeat(room)));
            spans.push(Span::styled(global, self.dim()));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_message(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.message {
            Some(m) if m.error => Line::styled(m.text.clone(), self.theme.fg(Color::Red)),
            Some(m) => Line::styled(m.text.clone(), self.theme.fg(Color::Green)),
            None => Line::default(),
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    /// The keys of the current tab, as `key what` pairs.
    fn keys(&self, keys: &[(&str, &str)]) -> Line<'static> {
        let mut spans = Vec::new();
        for (i, (key, what)) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(key.to_string(), self.bold()));
            spans.push(Span::styled(format!(" {what}"), self.dim()));
        }
        Line::from(spans)
    }
}

/// `~/…` for a path under the home directory.
fn tilde(store: &Store, path: &std::path::Path) -> String {
    match path.strip_prefix(&store.home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Raw mode and the alternate screen, given back on drop, on panic and while
/// a child run uses the terminal.
struct Screen {
    term: Terminal<CrosstermBackend<Stdout>>,
}

impl Screen {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let term = execute!(stdout(), terminal::EnterAlternateScreen)
            .and_then(|()| Terminal::new(CrosstermBackend::new(stdout())));
        match term {
            Ok(term) => Ok(Self { term }),
            Err(e) => {
                let _ = restore();
                Err(e)
            }
        }
    }

    /// Runs `hats <action> <label>`: add in the background, login and remove
    /// on the normal screen. The outer error means the terminal could not be
    /// taken back; the inner one is what to tell the user.
    fn run(&mut self, child: &Child) -> io::Result<Result<(), String>> {
        let mut cmd = match child.command() {
            Ok(cmd) => cmd,
            Err(e) => return Ok(Err(format!("could not find hats itself: {e}"))),
        };
        let what = format!("hats {} {}", child.action.command(), child.label);
        if !child.action.interactive() {
            return Ok(match cmd.stdin(Stdio::null()).output() {
                Ok(out) if out.status.success() => Ok(()),
                Ok(out) => Err(cli_error(&out.stderr)),
                Err(e) => Err(format!("could not start {what}: {e}")),
            });
        }
        self.term.show_cursor()?;
        restore()?;
        let outcome = match cmd.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("{what} failed ({status})")),
            Err(e) => Err(format!("could not start {what}: {e}")),
        };
        if let Err(e) = &outcome {
            // What the child printed is on this screen; let the user read it.
            print!("\n{e}. Press Enter to go back to hats.");
            stdout().flush()?;
            io::stdin().read_line(&mut String::new())?;
        }
        terminal::enable_raw_mode()?;
        execute!(stdout(), terminal::EnterAlternateScreen)?;
        // The next draw writes the whole screen. `Terminal::clear` would do
        // it too but first asks the terminal where the cursor is.
        let area = self.term.size()?.into();
        self.term.resize(area)?;
        Ok(outcome)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // Shown here so the terminal's own drop writes nothing after leaving.
        let _ = self.term.show_cursor();
        let _ = restore();
    }
}

fn restore() -> io::Result<()> {
    let raw = terminal::disable_raw_mode();
    execute!(stdout(), cursor::Show, terminal::LeaveAlternateScreen)?;
    raw
}

/// Gives the terminal back before the panic message prints. Release builds
/// abort on panic, so this is the only cleanup that runs.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        previous(info);
    }));
}

/// The CLI's own error line, without its `hats: ` prefix.
fn cli_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    text.strip_prefix("hats: ")
        .unwrap_or(text)
        .replace('\n', " ")
}

#[cfg(test)]
mod tests;
