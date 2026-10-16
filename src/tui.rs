//! Interactive view: the accounts of both harnesses with their limits, and
//! the CLI's actions on keys. It blocks on the next terminal event and draws
//! only after one that changed something, so it costs nothing while idle.

use std::io::{self, Stdout, Write, stdout};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{Frame, Terminal};

use crate::account::{DEFAULT_LABEL, Harness};
use crate::auto::{self, Status};
use crate::config::{self, Config, Mode, Relaunch, Strategy};
use crate::registry::{self, Change};
use crate::store::Store;
use crate::view;

/// Changes of account shown at the foot.
const RECENT: usize = 3;

pub fn run(store: &Store) -> Result<(), String> {
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let mut app = App::new(store, color);
    install_panic_hook();
    let mut screen =
        Screen::enter().map_err(|e| format!("could not open the interactive view: {e}"))?;
    let result = event_loop(&mut app, &mut screen);
    drop(screen);
    result.map_err(|e| e.to_string())
}

fn event_loop(app: &mut App, screen: &mut Screen) -> io::Result<()> {
    screen.term.draw(|frame| app.draw(frame))?;
    loop {
        // No timeout: nothing runs until the user does something.
        let step = match event::read()? {
            Event::Key(key) => app.on_key(key),
            Event::Resize(..) => Step::Redraw,
            _ => Step::Idle,
        };
        match step {
            Step::Idle => continue,
            Step::Redraw => {}
            Step::Quit => return Ok(()),
            Step::Run(child) => {
                let outcome = screen.run(&child)?;
                app.finish(&child, outcome);
            }
        }
        screen.term.draw(|frame| app.draw(frame))?;
    }
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

/// What sits between the list and the status line, or replaces the status line.
#[derive(Debug, Default, PartialEq)]
enum Panel {
    #[default]
    List,
    Settings(Field),
    /// Typing the label of a new account.
    Add(String),
    /// Waiting for y/n before removing this label.
    Remove(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Mode,
    Threshold,
    Strategy,
    Relaunch,
    Order,
}

impl Field {
    const ALL: [Field; 5] = [
        Field::Mode,
        Field::Threshold,
        Field::Strategy,
        Field::Relaunch,
        Field::Order,
    ];

    /// The key in `config.toml` and `config::KEYS`.
    fn key(self) -> &'static str {
        match self {
            Field::Mode => "mode",
            Field::Threshold => "threshold",
            Field::Strategy => "strategy",
            Field::Relaunch => "relaunch",
            Field::Order => "order",
        }
    }

    fn step(self, by: isize) -> Self {
        let i = Self::ALL.iter().position(|f| *f == self).expect("in ALL") as isize;
        Self::ALL[(i + by).rem_euclid(Self::ALL.len() as isize) as usize]
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
    focus: usize,
    selected: usize,
    panel: Panel,
    message: Option<Message>,
    /// The newest changes of account, of both harnesses.
    changes: Vec<Change>,
    color: bool,
}

impl<'a> App<'a> {
    fn new(store: &'a Store, color: bool) -> Self {
        let mut app = Self {
            store,
            config: Config::default(),
            groups: Harness::ALL.map(|harness| Group {
                harness,
                statuses: Vec::new(),
                current: None,
            }),
            focus: 0,
            selected: 0,
            panel: Panel::List,
            message: None,
            changes: Vec::new(),
            color,
        };
        app.reload();
        if app.groups[0].statuses.is_empty() && !app.groups[1].statuses.is_empty() {
            app.focus = 1;
        }
        app.select_current();
        app
    }

    fn harness(&self) -> Harness {
        self.groups[self.focus].harness
    }

    fn selected_status(&self) -> Option<&Status> {
        self.groups[self.focus].statuses.get(self.selected)
    }

    /// Reads config, accounts and limits again, keeping the selected account.
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
        self.changes = registry::recent(&self.store.registry(), 2 * RECENT);
        if let Some(e) = error {
            self.fail(e);
        }
        self.select_label(selected.as_deref());
    }

    /// Selects `label` if it is listed, else keeps the position in range.
    fn select_label(&mut self, label: Option<&str>) {
        let statuses = &self.groups[self.focus].statuses;
        self.selected = label
            .and_then(|l| statuses.iter().position(|s| s.account.label == l))
            .unwrap_or_else(|| self.selected.min(statuses.len().saturating_sub(1)));
    }

    fn select_current(&mut self) {
        self.selected = 0;
        let current = self.groups[self.focus].current.clone();
        self.select_label(current.as_deref());
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
            return Step::Quit;
        }
        let cleared = self.message.take().is_some();
        let (panel, step) = match std::mem::take(&mut self.panel) {
            Panel::List => self.list_key(key),
            Panel::Settings(field) => self.settings_key(field, key),
            Panel::Add(input) => self.add_key(input, key),
            Panel::Remove(label) => self.remove_key(label, key),
        };
        self.panel = panel;
        match step {
            Step::Idle if cleared => Step::Redraw,
            step => step,
        }
    }

    /// Keys that do the same in the list and in the settings panel.
    fn shared_key(&mut self, key: KeyEvent) -> Option<Step> {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        Some(match key.code {
            KeyCode::Char('q') => Step::Quit,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = (self.focus + 1) % self.groups.len();
                self.select_current();
                Step::Redraw
            }
            KeyCode::Char('r') => {
                self.reload();
                if self.message.is_none() {
                    self.info("Reloaded.".to_owned());
                }
                Step::Redraw
            }
            KeyCode::Char('K') => self.move_account(-1),
            KeyCode::Char('J') => self.move_account(1),
            KeyCode::Up if shift => self.move_account(-1),
            KeyCode::Down if shift => self.move_account(1),
            _ => return None,
        })
    }

    fn list_key(&mut self, key: KeyEvent) -> (Panel, Step) {
        if let Some(step) = self.shared_key(key) {
            return (Panel::List, step);
        }
        let step = match key.code {
            KeyCode::Esc => Step::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Enter | KeyCode::Char('u') => self.use_selected(),
            KeyCode::Char('a') => return (Panel::Add(String::new()), Step::Redraw),
            KeyCode::Char('c') => return (Panel::Settings(Field::Mode), Step::Redraw),
            KeyCode::Char('l') => match self.selected_status() {
                Some(s) => Step::Run(Child {
                    action: Action::Login,
                    harness: self.harness(),
                    label: s.account.label.clone(),
                }),
                None => self.nothing_selected(),
            },
            KeyCode::Char('d') => return self.ask_remove(),
            _ => Step::Idle,
        };
        (Panel::List, step)
    }

    fn settings_key(&mut self, field: Field, key: KeyEvent) -> (Panel, Step) {
        if let Some(step) = self.shared_key(key) {
            return (Panel::Settings(field), step);
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let by = if shift { 5 } else { 1 };
        let step = match key.code {
            KeyCode::Esc | KeyCode::Char('c') => return (Panel::List, Step::Redraw),
            KeyCode::Up | KeyCode::Char('k') => {
                return (Panel::Settings(field.step(-1)), Step::Redraw);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                return (Panel::Settings(field.step(1)), Step::Redraw);
            }
            // On the order row, left and right pick the account to move.
            KeyCode::Left if field == Field::Order && shift => self.move_account(-1),
            KeyCode::Right if field == Field::Order && shift => self.move_account(1),
            KeyCode::Left if field == Field::Order => self.move_selection(-1),
            KeyCode::Right if field == Field::Order => self.move_selection(1),
            KeyCode::Left => self.change(field, -by),
            KeyCode::Right => self.change(field, by),
            KeyCode::PageDown => self.change(field, -5),
            KeyCode::PageUp => self.change(field, 5),
            _ => Step::Idle,
        };
        (Panel::Settings(field), step)
    }

    fn add_key(&mut self, mut input: String, key: KeyEvent) -> (Panel, Step) {
        match key.code {
            KeyCode::Esc => (Panel::List, Step::Redraw),
            KeyCode::Enter if input.is_empty() => (Panel::List, Step::Redraw),
            KeyCode::Enter => {
                let child = Child {
                    action: Action::Add,
                    harness: self.harness(),
                    label: input,
                };
                (Panel::List, Step::Run(child))
            }
            KeyCode::Backspace => {
                input.pop();
                (Panel::Add(input), Step::Redraw)
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(c);
                (Panel::Add(input), Step::Redraw)
            }
            _ => (Panel::Add(input), Step::Idle),
        }
    }

    fn remove_key(&mut self, label: String, key: KeyEvent) -> (Panel, Step) {
        let step = match key.code {
            KeyCode::Char('y' | 'Y') => Step::Run(Child {
                action: Action::Remove,
                harness: self.harness(),
                label,
            }),
            _ => Step::Redraw,
        };
        (Panel::List, step)
    }

    fn move_selection(&mut self, by: isize) -> Step {
        let len = self.groups[self.focus].statuses.len();
        match self.selected.checked_add_signed(by).filter(|i| *i < len) {
            Some(i) => {
                self.selected = i;
                Step::Redraw
            }
            None => Step::Idle,
        }
    }

    fn nothing_selected(&mut self) -> Step {
        self.fail(format!(
            "No {} accounts yet; press a to add one.",
            self.harness().name()
        ));
        Step::Redraw
    }

    fn use_selected(&mut self) -> Step {
        let Some(s) = self.selected_status() else {
            return self.nothing_selected();
        };
        let (account, id) = (s.account.clone(), s.id.clone());
        let Some(id) = id else {
            self.fail(format!(
                "'{}' is not signed in; press l to sign in.",
                account.label
            ));
            return Step::Redraw;
        };
        match auto::choose(self.store, &account, &id, SystemTime::now()) {
            Ok(()) => self.info(format!(
                "New {} sessions use '{}'.",
                account.harness.name(),
                account.label
            )),
            Err(e) => self.fail(e),
        }
        self.reload();
        Step::Redraw
    }

    fn ask_remove(&mut self) -> (Panel, Step) {
        let Some(s) = self.selected_status() else {
            return (Panel::List, self.nothing_selected());
        };
        let (label, is_default) = (s.account.label.clone(), s.account.is_default);
        if is_default {
            self.fail(format!(
                "'{DEFAULT_LABEL}' is the harness's own directory; hats never removes it."
            ));
            return (Panel::List, Step::Redraw);
        }
        (Panel::Remove(label), Step::Redraw)
    }

    fn change(&mut self, field: Field, by: i16) -> Step {
        let c = self.config.for_harness(self.harness());
        let value = match field {
            Field::Mode => flip(c.mode, Mode::Auto, Mode::Manual).as_str().to_owned(),
            Field::Strategy => flip(c.strategy, Strategy::Headroom, Strategy::Next)
                .as_str()
                .to_owned(),
            Field::Relaunch => flip(c.relaunch, Relaunch::Idle, Relaunch::Off)
                .as_str()
                .to_owned(),
            Field::Threshold => {
                let next = (i16::from(c.threshold) + by).clamp(1, 100);
                if next == i16::from(c.threshold) {
                    return Step::Idle;
                }
                next.to_string()
            }
            Field::Order => return Step::Idle,
        };
        self.save(field.key(), &value)
    }

    /// Moves the selected account `by` places in the rotation order.
    fn move_account(&mut self, by: isize) -> Step {
        let statuses = &self.groups[self.focus].statuses;
        let from = self.selected;
        let Some(to) = from
            .checked_add_signed(by)
            .filter(|to| *to < statuses.len())
        else {
            return Step::Idle;
        };
        let mut labels: Vec<&str> = statuses.iter().map(|s| s.account.label.as_str()).collect();
        labels.swap(from, to);
        let moved = labels[to].to_owned();
        let order = labels.join(",");
        let step = self.save("order", &order);
        self.select_label(Some(&moved));
        step
    }

    /// Writes one key of the focused harness and reads everything back.
    fn save(&mut self, key: &str, value: &str) -> Step {
        let full = format!("{}.{key}", self.harness().name());
        match config::set(&self.store.config, &full, value) {
            Ok(()) => self.info(format!("Saved {full} = {value}.")),
            Err(e) => self.fail(e),
        }
        self.reload();
        Step::Redraw
    }

    /// Takes in what a child run did.
    fn finish(&mut self, child: &Child, outcome: Result<(), String>) {
        self.reload();
        let label = &child.label;
        if let Err(e) = outcome {
            self.fail(e);
            return;
        }
        match child.action {
            Action::Add => {
                self.select_label(Some(label));
                self.info(format!("Added '{label}'. Press l to sign in."));
            }
            Action::Login => {
                let signed_in = self.groups[self.focus]
                    .statuses
                    .iter()
                    .any(|s| s.account.label == *label && s.id.is_some());
                if signed_in {
                    self.info(format!("'{label}' is signed in."));
                } else {
                    self.fail(format!("'{label}' is still not signed in."));
                }
            }
            Action::Remove => self.info(format!("Removed '{label}'.")),
        }
    }

    // Drawing.

    fn fg(&self, color: Color) -> Style {
        if self.color {
            Style::new().fg(color)
        } else {
            Style::new()
        }
    }

    fn dim(&self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    fn draw(&self, frame: &mut Frame) {
        let now = SystemTime::now();
        let panel_height = if matches!(self.panel, Panel::Settings(_)) {
            Field::ALL.len() as u16 + 3
        } else {
            0
        };
        let changes = self.recent_changes(now);
        let [list, panel, recent, status, footer] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(panel_height),
            Constraint::Length(changes.len() as u16),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_list(frame, list, now);
        if let Panel::Settings(field) = self.panel {
            self.draw_settings(frame, panel, field);
        }
        frame.render_widget(Paragraph::new(changes), recent);
        self.draw_status(frame, status);
        frame.render_widget(Paragraph::new(self.help()), footer);
    }

    /// The focused harness's last changes of account, in words, as the
    /// registry recorded them.
    fn recent_changes(&self, now: SystemTime) -> Vec<Line<'static>> {
        let harness = self.harness();
        let mut lines: Vec<Line> = self
            .changes
            .iter()
            .filter(|c| c.harness == harness)
            .rev()
            .take(RECENT)
            .map(|c| Line::styled(view::change(c, now), self.dim()))
            .collect();
        lines.reverse();
        if lines.is_empty() {
            lines.push(Line::styled("No change of account yet.", self.dim()));
        }
        lines
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect, now: SystemTime) {
        let width = usize::from(area.width);
        let mut lines = Vec::new();
        let mut selected = 0..0;
        for (i, group) in self.groups.iter().enumerate() {
            let focused = i == self.focus;
            if !focused && group.statuses.is_empty() {
                continue;
            }
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            if group.statuses.is_empty() {
                lines.push(Line::styled(
                    view::title(group.harness),
                    self.tone(view::Tone::Title, focused),
                ));
                lines.push(Line::styled(
                    format!(
                        "  No {} accounts yet. Press a to add one.",
                        group.harness.name()
                    ),
                    self.dim(),
                ));
                continue;
            }
            let block = view::block(
                group.harness,
                self.config.for_harness(group.harness),
                &group.statuses,
                group.current.as_deref(),
                width,
                now,
                view::Hints::Keys,
            );
            lines.extend(block.header.iter().map(|row| self.line(row, focused)));
            for (j, rows) in block.accounts.iter().enumerate() {
                let start = lines.len();
                for row in rows {
                    let mut line = self.line(row, focused);
                    if focused && j == self.selected {
                        line = line.patch_style(Style::new().add_modifier(Modifier::REVERSED));
                    }
                    lines.push(line);
                }
                if focused && j == self.selected {
                    selected = start..lines.len();
                }
            }
        }
        let scroll = selected.end.saturating_sub(usize::from(area.height));
        frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), area);
    }

    fn line(&self, row: &view::Row, focused: bool) -> Line<'static> {
        Line::from(
            row.iter()
                .map(|c| Span::styled(c.text.clone(), self.tone(c.tone, focused)))
                .collect::<Vec<_>>(),
        )
    }

    fn tone(&self, tone: view::Tone, focused: bool) -> Style {
        match tone {
            view::Tone::Plain => Style::new(),
            view::Tone::Dim => self.dim(),
            view::Tone::Strong => self.bold(),
            view::Tone::Title if focused => self.bold().patch(self.fg(Color::Cyan)),
            view::Tone::Title => self.bold(),
            view::Tone::Good => self.fg(Color::Green),
            view::Tone::Warn => self.fg(Color::Yellow),
            view::Tone::Bad => self.fg(Color::Red),
        }
    }

    fn draw_settings(&self, frame: &mut Frame, area: Rect, field: Field) {
        let harness = self.harness();
        let c = self.config.for_harness(harness);
        let mut lines: Vec<Line> = Field::ALL
            .iter()
            .map(|&f| {
                let focused = f == field;
                let mut spans = vec![
                    Span::raw(if focused { "› " } else { "  " }),
                    Span::raw(format!("{:<11}", f.key())),
                ];
                let value = match f {
                    Field::Mode => c.mode.as_str().to_owned(),
                    Field::Threshold => format!("{} %", c.threshold),
                    Field::Strategy => c.strategy.as_str().to_owned(),
                    Field::Relaunch => c.relaunch.as_str().to_owned(),
                    Field::Order => {
                        spans.extend(self.order_spans(focused));
                        return Line::from(spans);
                    }
                };
                if focused {
                    spans.push(Span::styled(format!("‹ {value} ›"), self.bold()));
                } else {
                    spans.push(Span::raw(format!("  {value}")));
                }
                Line::from(spans)
            })
            .collect();
        let help = config::KEYS
            .iter()
            .find(|(k, _)| *k == field.key())
            .map_or("", |(_, h)| *h);
        lines.push(Line::styled(format!("  {help}"), self.dim()));
        let block = Block::bordered().title(format!(" {} settings ", harness.name()));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// The accounts in rotation order, the selected one marked.
    fn order_spans(&self, focused: bool) -> Vec<Span<'static>> {
        let mut spans = vec![Span::raw("  ")];
        for (i, s) in self.groups[self.focus].statuses.iter().enumerate() {
            let style = match (i == self.selected, focused) {
                (true, true) => Style::new().add_modifier(Modifier::REVERSED),
                (true, false) => self.bold(),
                _ => Style::new(),
            };
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("{}. {}", i + 1, s.account.label),
                style,
            ));
        }
        spans
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.panel {
            Panel::Add(input) => {
                let prompt = format!("New {} account label: ", self.harness().name());
                let x = (prompt.chars().count() + input.chars().count()) as u16;
                frame.set_cursor_position(Position::new(
                    area.x + x.min(area.width.saturating_sub(1)),
                    area.y,
                ));
                Line::from(format!("{prompt}{input}"))
            }
            Panel::Remove(label) => Line::styled(
                format!("Remove '{label}'? It signs out and deletes its directory. y/n"),
                self.bold().patch(self.fg(Color::Yellow)),
            ),
            _ => match &self.message {
                Some(m) if m.error => Line::styled(m.text.clone(), self.fg(Color::Red)),
                Some(m) => Line::styled(m.text.clone(), self.fg(Color::Green)),
                None => Line::default(),
            },
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn help(&self) -> Line<'static> {
        let other = self.groups[(self.focus + 1) % self.groups.len()]
            .harness
            .name();
        let keys: &[(&str, &str)] = match self.panel {
            Panel::List => &[
                ("↑↓", "move"),
                ("tab", other),
                ("u", "use"),
                ("a", "add"),
                ("l", "login"),
                ("d", "remove"),
                ("c", "config"),
                ("r", "reload"),
                ("q", "quit"),
            ],
            Panel::Settings(Field::Order) => &[
                ("↑↓", "setting"),
                ("←→", "pick account"),
                ("J/K", "move it"),
                ("tab", other),
                ("esc", "close"),
                ("q", "quit"),
            ],
            Panel::Settings(_) => &[
                ("↑↓", "setting"),
                ("←→", "change"),
                ("shift ←→", "±5"),
                ("J/K", "order"),
                ("tab", other),
                ("esc", "close"),
                ("q", "quit"),
            ],
            Panel::Add(_) => &[("enter", "create"), ("esc", "cancel")],
            Panel::Remove(_) => &[("y", "remove"), ("n", "keep")],
        };
        let mut spans = Vec::new();
        for (i, (key, what)) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(*key, self.bold()));
            spans.push(Span::styled(format!(" {what}"), self.dim()));
        }
        Line::from(spans)
    }
}

fn flip<T: PartialEq>(value: T, a: T, b: T) -> T {
    if value == a { b } else { a }
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
mod tests {
    use std::fs;

    use ratatui::backend::TestBackend;

    use super::*;
    use crate::account::Account;
    use crate::limits::{self, LimitWindow, unix_secs};

    fn sandbox() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = Store {
            home: root.join("home"),
            data: root.join("data"),
            state: root.join("state"),
            config: root.join("config/hats/config.toml"),
        };
        (dir, store)
    }

    /// A Claude account, signed in with limits when `used` is given.
    fn account(store: &Store, label: &str, used: Option<f64>) -> Account {
        let account = Account::managed(Harness::Claude, label, store).unwrap();
        account.create(store).unwrap();
        if let Some(used) = used {
            let identity = format!(
                r#"{{"oauthAccount":{{"accountUuid":"{label}","organizationUuid":"o","emailAddress":"{label}@example.com","organizationType":"claude_team","organizationName":"Acme"}}}}"#
            );
            fs::write(account.dir.join(".claude.json"), identity).unwrap();
            let id = account.provider_id(store).unwrap().unwrap();
            let now = SystemTime::now();
            let window = |name: &str, used_percent| LimitWindow {
                name: name.to_owned(),
                used_percent,
                resets_at: unix_secs(now) + 3600,
            };
            let windows = vec![window("five_hour", used), window("seven_day", 12.0)];
            limits::save(store, &account, &id, windows, now).unwrap();
        }
        account
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn render(app: &App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|frame| app.draw(frame)).unwrap();
        term
    }

    fn text(app: &App) -> String {
        text_at(app, 100)
    }

    fn text_at(app: &App, width: u16) -> String {
        let term = render(app, width, 16);
        let buffer = term.backend().buffer();
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                let row: String = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_with<'t>(screen: &'t str, needle: &str) -> &'t str {
        screen
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle:?} in\n{screen}"))
    }

    #[test]
    fn shows_accounts_limits_and_keys() {
        let (_dir, store) = sandbox();
        let work = account(&store, "work", Some(42.0));
        account(&store, "personal", None);
        let id = work.provider_id(&store).unwrap().unwrap();
        auto::choose(&store, &work, &id, SystemTime::now()).unwrap();

        let mut app = App::new(&store, true);
        let screen = text(&app);
        assert!(screen.contains("CLAUDE CODE · new sessions start in ● work"));
        assert!(screen.contains(
            "Auto: at 90 % of either limit, it moves to the account with the most room."
        ));
        for title in ["ACCOUNT", "5-HOUR LIMIT", "WEEKLY LIMIT", "UPDATED"] {
            assert!(line_with(&screen, "ACCOUNT").contains(title), "{title}");
        }
        let row = line_with(&screen, " work ");
        assert!(row.starts_with("● work"), "{row}");
        for part in ["▰▰", " 42% ", " 12% ", "just now"] {
            assert!(row.contains(part), "{part:?} in {row}");
        }
        assert!(
            screen.contains("work@example.com · Team · Acme"),
            "{screen}"
        );
        assert!(
            line_with(&screen, "personal").contains("not signed in: press l to sign in"),
            "{screen}"
        );
        assert!(!screen.contains("signed in   "), "no status column");
        // The foot says what changed, as the registry recorded it.
        assert!(
            line_with(&screen, "→ work").ends_with("default → work: you chose it"),
            "{screen}"
        );
        assert!(!screen.contains("CODEX"), "no codex accounts:\n{screen}");
        assert!(screen.lines().last().unwrap().contains("q quit"));

        assert_eq!(app.on_key(key(KeyCode::Tab)), Step::Redraw);
        let screen = text(&app);
        assert!(screen.contains("CODEX") && screen.contains("No codex accounts yet"));
    }

    #[test]
    fn settings_are_saved_on_every_change() {
        let (_dir, store) = sandbox();
        account(&store, "work", None);
        let mut app = App::new(&store, true);
        let threshold = || Config::load(&store.config).unwrap().claude.threshold;

        app.on_key(key(KeyCode::Char('c')));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.panel, Panel::Settings(Field::Threshold));
        app.on_key(key(KeyCode::Right));
        assert_eq!(threshold(), 91);
        app.on_key(shift(KeyCode::Right));
        assert_eq!(threshold(), 96);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(threshold(), 100);
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.on_key(key(KeyCode::Right)), Step::Idle, "already 100");
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(threshold(), 95);
        assert!(text(&app).contains("Auto: at 95 % of either limit"));

        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Left));
        let config = Config::load(&store.config).unwrap();
        assert_eq!(config.claude.mode, Mode::Manual);
        assert_eq!(config.codex, Default::default(), "only the focused harness");

        assert_eq!(app.on_key(key(KeyCode::Esc)), Step::Redraw);
        assert_eq!(app.panel, Panel::List);
        assert_eq!(app.on_key(key(KeyCode::Esc)), Step::Quit);
    }

    #[test]
    fn order_moves_the_selected_account() {
        let (_dir, store) = sandbox();
        for label in ["a", "x", "y"] {
            account(&store, label, None);
        }
        let mut app = App::new(&store, true);
        let labels = |app: &App| -> Vec<String> {
            app.groups[0]
                .statuses
                .iter()
                .map(|s| s.account.label.clone())
                .collect()
        };
        let order = || Config::load(&store.config).unwrap().claude.order;
        // The harness's own directory exists, so `default` is listed by name.
        assert_eq!(labels(&app), ["a", "default", "x", "y"]);
        app.select_label(Some("x"));

        // `default` is ordered like any other account.
        app.on_key(key(KeyCode::Char('K')));
        assert_eq!(order(), ["a", "x", "default", "y"]);
        assert_eq!(labels(&app), ["a", "x", "default", "y"]);
        assert_eq!(app.selected_status().unwrap().account.label, "x");
        app.on_key(key(KeyCode::Char('J')));
        assert_eq!(order(), ["a", "default", "x", "y"]);

        app.on_key(shift(KeyCode::Up));
        assert_eq!(order(), ["a", "x", "default", "y"]);
        assert_eq!(app.selected, 1);

        app.on_key(key(KeyCode::Char('c')));
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.panel, Panel::Settings(Field::Order));
        app.on_key(key(KeyCode::Left));
        app.on_key(shift(KeyCode::Right));
        assert_eq!(order(), ["x", "a", "default", "y"]);
    }

    #[test]
    fn use_and_the_cli_actions() {
        let (_dir, store) = sandbox();
        account(&store, "work", Some(10.0));
        account(&store, "personal", None);
        let mut app = App::new(&store, true);
        let current = || store.current(Harness::Claude).unwrap().label;
        let select = |app: &mut App, label: &str| {
            app.select_label(Some(label));
            assert_eq!(app.selected_status().unwrap().account.label, label);
        };

        select(&mut app, "personal");
        app.on_key(key(KeyCode::Enter));
        assert!(app.message.as_ref().unwrap().text.contains("not signed in"));
        assert_eq!(current(), "default");
        select(&mut app, "work");
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(current(), "work");

        let child = |action, label: &str| {
            Step::Run(Child {
                action,
                harness: Harness::Claude,
                label: label.to_owned(),
            })
        };
        assert_eq!(
            app.on_key(key(KeyCode::Char('l'))),
            child(Action::Login, "work")
        );
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.panel, Panel::Remove("work".to_owned()));
        assert_eq!(app.on_key(key(KeyCode::Char('n'))), Step::Redraw);
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            child(Action::Remove, "work")
        );
        select(&mut app, "default");
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.panel, Panel::List, "default is never removed");

        app.on_key(key(KeyCode::Char('a')));
        for c in "nex".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Char('w')));
        assert!(text(&app).contains("New claude account label: new"));
        assert_eq!(app.on_key(key(KeyCode::Enter)), child(Action::Add, "new"));
    }

    #[test]
    fn fits_80_columns_and_uses_120() {
        let (_dir, store) = sandbox();
        account(&store, "work", Some(42.0));
        account(&store, "personal", None);
        let app = App::new(&store, true);

        // 80 columns: who it is goes on a second line, nothing is cut.
        let narrow = text_at(&app, 80);
        let row = line_with(&narrow, " work ");
        assert!(row.ends_with("just now"), "{row}");
        assert!(!row.contains('@'), "{row}");
        assert!(
            narrow.contains("\n    work@example.com · Team · Acme"),
            "{narrow}"
        );
        assert!(narrow.lines().all(|l| l.chars().count() <= 80));

        // 120 columns: one line per account, with its own column.
        let wide = text_at(&app, 120);
        assert!(
            line_with(&wide, "ACCOUNT").contains("SIGNED IN AS"),
            "{wide}"
        );
        let row = line_with(&wide, " work ");
        assert!(
            row.contains("work@example.com · Team · Acme") && row.ends_with("just now"),
            "{row}"
        );
    }

    #[test]
    fn no_color_draws_without_colors() {
        let (_dir, store) = sandbox();
        account(&store, "work", Some(95.0));
        let colors = |color: bool| {
            let app = App::new(&store, color);
            let term = render(&app, 100, 16);
            term.backend()
                .buffer()
                .content()
                .iter()
                .filter(|cell| cell.fg != Color::Reset || cell.bg != Color::Reset)
                .count()
        };
        assert!(colors(true) > 0);
        assert_eq!(colors(false), 0);
    }
}
