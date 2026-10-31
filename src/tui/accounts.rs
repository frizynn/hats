//! The Accounts tab: one harness at a time, one line per account and the
//! selected account's details under them.

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListState, Paragraph};

use super::{Action, App, Child, Step, Tab};
use crate::account::{DEFAULT_LABEL, Harness};
use crate::auto::{self, Status};
use crate::config;
use crate::limits::{self, LimitWindow};
use crate::view::{self, Tone};

/// Marks the selected row, before the text.
pub const MARK: &str = "▌ ";

#[derive(Default)]
pub struct State {
    /// The harness shown, an index into `App::groups`.
    pub focus: usize,
    pub selected: usize,
    pub prompt: Option<Prompt>,
}

#[derive(Debug, PartialEq)]
pub enum Prompt {
    /// Typing the label of a new account.
    Add(String),
    /// Waiting for y/n before removing this label.
    Remove(String),
}

impl State {
    pub fn prompt_shown(&self, tab: Tab) -> bool {
        tab == Tab::Accounts && self.prompt.is_some()
    }
}

impl App<'_> {
    pub(super) fn harness(&self) -> Harness {
        self.groups[self.accounts.focus].harness
    }

    pub(super) fn selected_status(&self) -> Option<&Status> {
        self.groups[self.accounts.focus]
            .statuses
            .get(self.accounts.selected)
    }

    /// Selects `label` if it is listed, else keeps the position in range.
    pub(super) fn select_label(&mut self, label: Option<&str>) {
        let statuses = &self.groups[self.accounts.focus].statuses;
        self.accounts.selected = label
            .and_then(|l| statuses.iter().position(|s| s.account.label == l))
            .unwrap_or_else(|| self.accounts.selected.min(statuses.len().saturating_sub(1)));
    }

    pub(super) fn select_current(&mut self) {
        self.accounts.selected = 0;
        let current = self.groups[self.accounts.focus].current.clone();
        self.select_label(current.as_deref());
    }

    fn is_off(&self, label: &str) -> bool {
        self.config
            .for_harness(self.harness())
            .disabled
            .iter()
            .any(|l| l == label)
    }

    pub(super) fn accounts_key(&mut self, key: KeyEvent) -> Step {
        match self.accounts.prompt.take() {
            Some(Prompt::Add(input)) => return self.add_key(input, key),
            Some(Prompt::Remove(label)) => return self.remove_key(label, key),
            None => {}
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if shift => self.move_account(-1),
            KeyCode::Down if shift => self.move_account(1),
            KeyCode::Char('K') => self.move_account(-1),
            KeyCode::Char('J') => self.move_account(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Left | KeyCode::Right => {
                self.accounts.focus = (self.accounts.focus + 1) % self.groups.len();
                self.select_current();
                Step::Redraw
            }
            KeyCode::Enter | KeyCode::Char('u') => self.use_selected(),
            KeyCode::Char(' ') => self.toggle_selected(),
            KeyCode::Char('a') => {
                self.accounts.prompt = Some(Prompt::Add(String::new()));
                Step::Redraw
            }
            KeyCode::Char('l') => match self.selected_status() {
                Some(s) => Step::Run(Child {
                    action: Action::Login,
                    harness: self.harness(),
                    label: s.account.label.clone(),
                }),
                None => self.nothing_selected(),
            },
            KeyCode::Char('d') => self.ask_remove(),
            _ => Step::Idle,
        }
    }

    fn add_key(&mut self, mut input: String, key: KeyEvent) -> Step {
        match key.code {
            KeyCode::Esc => Step::Redraw,
            KeyCode::Enter if input.is_empty() => Step::Redraw,
            KeyCode::Enter => Step::Run(Child {
                action: Action::Add,
                harness: self.harness(),
                label: input,
            }),
            KeyCode::Backspace => {
                input.pop();
                self.accounts.prompt = Some(Prompt::Add(input));
                Step::Redraw
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(c);
                self.accounts.prompt = Some(Prompt::Add(input));
                Step::Redraw
            }
            _ => {
                self.accounts.prompt = Some(Prompt::Add(input));
                Step::Idle
            }
        }
    }

    fn remove_key(&mut self, label: String, key: KeyEvent) -> Step {
        match key.code {
            KeyCode::Char('y' | 'Y') => Step::Run(Child {
                action: Action::Remove,
                harness: self.harness(),
                label,
            }),
            _ => Step::Redraw,
        }
    }

    fn move_selection(&mut self, by: isize) -> Step {
        let len = self.groups[self.accounts.focus].statuses.len();
        match self
            .accounts
            .selected
            .checked_add_signed(by)
            .filter(|i| *i < len)
        {
            Some(i) => {
                self.accounts.selected = i;
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
            Ok(()) => {
                if self.is_off(&account.label) {
                    self.fail(crate::off_warning(account.harness, &account.label));
                } else {
                    self.info(view::chosen(account.harness, &account.label));
                }
            }
            Err(e) => self.fail(e),
        }
        self.reload();
        Step::Redraw
    }

    /// Turns the selected account on or off for auto switching.
    fn toggle_selected(&mut self) -> Step {
        let Some(s) = self.selected_status() else {
            return self.nothing_selected();
        };
        let label = s.account.label.clone();
        let harness = self.harness();
        let on = self.is_off(&label);
        match config::set_enabled(&self.store.config, harness, &label, on) {
            Ok(()) if on => self.info(format!(
                "'{label}' is enabled: auto switching may move to it."
            )),
            Ok(()) => {
                let moved = config::Config::load(&self.store.config)
                    .map_err(|e| e.to_string())
                    .and_then(|c| {
                        auto::leave_disabled(self.store, harness, c.for_harness(harness), &label)
                    });
                match moved {
                    Ok(Some((to, _))) => self.info(format!(
                        "'{label}' is disabled; moved to '{}'. {}",
                        to.label,
                        view::chosen(harness, &to.label)
                    )),
                    Ok(None) => self.info(format!(
                        "'{label}' is disabled: auto switching never moves to it; enter still uses it."
                    )),
                    Err(e) => self.fail(e),
                }
            }
            Err(e) => self.fail(e),
        }
        self.reload();
        Step::Redraw
    }

    fn ask_remove(&mut self) -> Step {
        let Some(s) = self.selected_status() else {
            return self.nothing_selected();
        };
        if s.account.is_default {
            self.fail(format!(
                "'{DEFAULT_LABEL}' is the harness's own directory; hats never removes it."
            ));
            return Step::Redraw;
        }
        self.accounts.prompt = Some(Prompt::Remove(s.account.label.clone()));
        Step::Redraw
    }

    /// Moves the selected account `by` places in the rotation order.
    fn move_account(&mut self, by: isize) -> Step {
        let statuses = &self.groups[self.accounts.focus].statuses;
        let from = self.accounts.selected;
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
        let full = format!("{}.order", self.harness().name());
        match config::set(&self.store.config, &full, &order) {
            Ok(()) => self.info(format!("Saved the order: {}.", order.replace(',', ", "))),
            Err(e) => self.fail(e),
        }
        self.reload();
        self.select_label(Some(&moved));
        Step::Redraw
    }

    /// Takes in what a child run did.
    pub(super) fn finish(&mut self, child: &Child, outcome: Result<(), String>) {
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
                let signed_in = self.groups[self.accounts.focus]
                    .statuses
                    .iter()
                    .find(|s| s.account.label == *label && s.id.is_some())
                    .map(|s| (s.account.clone(), s.id.clone(), s.login_refused()));
                if let Some((account, Some(id), refused)) = signed_in {
                    // A new login: no longer the refused one, and a poll now
                    // says whether the provider takes it.
                    if refused {
                        let _ = limits::clear_refused(self.store, &account, &id);
                        self.reload();
                    }
                    super::poll_now();
                    self.info(format!("'{label}' is signed in."));
                } else {
                    self.fail(format!("'{label}' is still not signed in."));
                }
            }
            Action::Remove => self.info(format!("Removed '{label}'.")),
        }
    }

    // Drawing.

    /// Draws the tab and its prompt; returns its keys.
    pub(super) fn draw_accounts(
        &self,
        frame: &mut Frame,
        area: Rect,
        message: Rect,
    ) -> Vec<(&'static str, &'static str)> {
        let now = SystemTime::now();
        let group = &self.groups[self.accounts.focus];
        let config = self.config.for_harness(group.harness);
        let width = usize::from(area.width).saturating_sub(MARK.chars().count());
        let layout = view::Layout::new([(group.harness, config, group.statuses.as_slice())], width)
            .one_line();
        let block = view::block(
            group.harness,
            config,
            &group.statuses,
            group.current.as_deref(),
            &layout,
            now,
            view::Hints::Keys,
        );
        let mut header = vec![self.harnesses()];
        // The first header line names the harness; the selector says it.
        for row in block.header.iter().skip(1) {
            let mut line = self.line(row);
            if row.first().is_some_and(|c| c.tone == Tone::Dim) {
                line.spans.insert(0, Span::raw("  "));
            }
            header.push(line);
        }
        let stuck = config.mode == crate::config::Mode::Auto
            && group.current.as_deref().is_some_and(|current| {
                !group.statuses.is_empty()
                    && !auto::has_destination(&group.statuses, current, config, now)
            });
        // Above the column titles, under the mode sentence.
        if stuck {
            let current = group.current.as_deref().unwrap_or(DEFAULT_LABEL);
            header.insert(
                header.len() - 1,
                Line::styled(view::stuck(current), self.theme.fg(Color::Yellow)),
            );
        }
        let rows = group.statuses.len().max(1) as u16;
        let [top, list, details] = Layout::vertical([
            Constraint::Length(header.len() as u16),
            Constraint::Length(rows),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(header), top);
        if group.statuses.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!(
                        "  No {} accounts yet. Press a to add one.",
                        group.harness.name()
                    ),
                    self.dim(),
                )),
                list,
            );
        } else {
            let items: Vec<Line> = block
                .accounts
                .iter()
                .map(|rows| self.line(&rows[0]))
                .collect();
            let list_widget = List::new(items)
                .highlight_symbol(Line::styled(MARK, self.theme.fg(Color::Cyan)))
                .highlight_spacing(HighlightSpacing::Always)
                .highlight_style(self.theme.selected());
            let mut state = ListState::default().with_selected(Some(self.accounts.selected));
            frame.render_stateful_widget(list_widget, list, &mut state);
        }
        if let Some(s) = self.selected_status() {
            self.draw_details(frame, details, s, now);
        }
        self.draw_prompt(frame, message);
        match &self.accounts.prompt {
            Some(Prompt::Add(_)) => vec![("enter", "create"), ("esc", "cancel")],
            Some(Prompt::Remove(_)) => vec![("y", "remove"), ("n", "keep")],
            None => {
                let other = match group.harness {
                    Harness::Claude => "codex",
                    Harness::Codex => "claude",
                };
                let toggle = match self.selected_status() {
                    Some(s) if self.is_off(&s.account.label) => "enable",
                    _ => "disable",
                };
                vec![
                    ("↑↓", "move"),
                    ("←→", other),
                    ("enter", "use"),
                    ("space", toggle),
                    ("⇧↑↓", "order"),
                    ("a", "add"),
                    ("l", "login"),
                    ("d", "remove"),
                ]
            }
        }
    }

    /// `Claude Code 6  Codex 2`, the shown one selected, and the account in
    /// use.
    fn harnesses(&self) -> Line<'static> {
        let mut spans = Vec::new();
        for (i, group) in self.groups.iter().enumerate() {
            let label = format!(
                " {} {} ",
                match group.harness {
                    Harness::Claude => "Claude Code",
                    Harness::Codex => "Codex",
                },
                group.statuses.len()
            );
            spans.push(if i == self.accounts.focus {
                Span::styled(label, self.theme.selected())
            } else {
                Span::styled(label, self.dim())
            });
            spans.push(Span::raw(" "));
        }
        let group = &self.groups[self.accounts.focus];
        spans.push(Span::raw(format!(" {} ", view::in_use(group.harness))));
        spans.push(Span::styled(view::MARK, self.theme.fg(Color::Cyan)));
        spans.push(Span::styled(
            format!(" {}", group.current.as_deref().unwrap_or(DEFAULT_LABEL)),
            self.bold(),
        ));
        Line::from(spans)
    }

    fn draw_details(&self, frame: &mut Frame, area: Rect, s: &Status, now: SystemTime) {
        if area.height < 2 {
            return;
        }
        let id = &s.identity;
        let dash = || "–".to_owned();
        let mut rows: Vec<(&str, Line)> = vec![
            (
                "Mail",
                Line::raw(match (&id.email, s.account.harness) {
                    (Some(mail), _) => mail.clone(),
                    // Codex keeps it next to its tokens, which hats never reads.
                    (None, Harness::Codex) => "not read: Codex keeps it with its login".to_owned(),
                    (None, Harness::Claude) => dash(),
                }),
            ),
            (
                "Organization",
                Line::raw(id.organization.clone().unwrap_or_else(dash)),
            ),
            ("Plan", Line::raw(id.plan.clone().unwrap_or_else(dash))),
            (
                "Folder",
                Line::raw(super::tilde(self.store, &s.account.dir)),
            ),
        ];
        if let Some(at) = s.reading.as_ref().and_then(|r| r.login_refused_at) {
            rows.push((
                "Login",
                Line::styled(
                    format!(
                        "expired: the provider refused it at {} · press l to sign in again",
                        view::clock(at, now)
                    ),
                    self.theme.fg(Color::Red),
                ),
            ));
        }
        let reading = s.reading.as_ref();
        let windows = reading.map(|r| r.windows.as_slice()).unwrap_or_default();
        let resets: Vec<String> = [("five_hour", "5-hour"), ("seven_day", "weekly")]
            .iter()
            .filter_map(|(name, what)| {
                let w = windows.iter().find(|w| w.name == *name)?;
                Some(format!("{what} {}", resets(w, now)))
            })
            .collect();
        rows.push((
            "Resets",
            Line::raw(match (s.id.is_some(), resets.is_empty()) {
                (false, _) => "not signed in: press l to sign in".to_owned(),
                (true, true) => "no limits seen yet".to_owned(),
                (true, false) => resets.join(" · "),
            }),
        ));
        // Only numbers have an age; a refusal or a limit error alone has none.
        if let Some(r) = reading.filter(|r| !r.windows.is_empty()) {
            rows.push(("Updated", Line::raw(view::age(r, now))));
        }
        let off = self.is_off(&s.account.label);
        rows.push((
            "Auto switch",
            if off {
                Line::styled(
                    "disabled: never moves here · space enables it",
                    self.theme.fg(Color::Yellow),
                )
            } else {
                Line::raw(
                    "enabled: may move here when the account in use fills · space disables it",
                )
            },
        ));
        let last = self.changes.iter().rev().find(|c| {
            c.harness == s.account.harness
                && (c.to == s.account.label || c.from.as_deref() == Some(&s.account.label))
        });
        rows.push((
            "Last change",
            Line::raw(last.map_or_else(|| "none yet".to_owned(), |c| view::change(c, now))),
        ));
        let lines: Vec<Line> = rows
            .into_iter()
            .map(|(name, value)| {
                let mut spans = vec![Span::styled(format!(" {name:<13}"), self.dim())];
                // The value's own color, such as the red of an expired login.
                spans.extend(value.spans.into_iter().map(|s| s.patch_style(value.style)));
                Line::from(spans)
            })
            .collect();
        let title = Line::from(vec![
            Span::raw("─ "),
            Span::styled(s.account.label.clone(), self.bold()),
            Span::raw(" "),
        ]);
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(self.dim())
            .title(title);
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_prompt(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.accounts.prompt {
            Some(Prompt::Add(input)) => {
                let prompt = format!("New {} account label: ", self.harness().name());
                let x = (prompt.chars().count() + input.chars().count()) as u16;
                frame.set_cursor_position(Position::new(
                    area.x + x.min(area.width.saturating_sub(1)),
                    area.y,
                ));
                Line::from(format!("{prompt}{input}"))
            }
            Some(Prompt::Remove(label)) => Line::styled(
                format!("Remove '{label}'? It signs out and deletes its directory. y/n"),
                self.bold().patch(self.theme.fg(Color::Yellow)),
            ),
            None => return,
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    pub(super) fn line(&self, row: &view::Row) -> Line<'static> {
        Line::from(
            row.iter()
                .map(|c| Span::styled(c.text.clone(), self.tone(c.tone)))
                .collect::<Vec<_>>(),
        )
    }

    pub(super) fn tone(&self, tone: Tone) -> Style {
        match tone {
            Tone::Plain => Style::new(),
            Tone::Dim => self.dim(),
            Tone::Strong => self.bold(),
            Tone::Title => self.theme.accent(),
            Tone::Good => self.theme.fg(Color::Green),
            Tone::Warn => self.theme.fg(Color::Yellow),
            Tone::Bad => self.theme.fg(Color::Red),
        }
    }
}

/// `21 % until 17:24`, or `0 %` while the window is empty.
fn resets(w: &LimitWindow, now: SystemTime) -> String {
    let label = view::reset_label(w.resets_at, now);
    if label.is_empty() {
        return "0 %".to_owned();
    }
    format!("{:.0} % until {label}", view::used_now(w, now))
}
