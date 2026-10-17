//! The Settings tab: every key of `config.toml` for both harnesses with its
//! value and its help, changed in place and saved at once, and where hats
//! keeps its files.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListState, Paragraph, Wrap};

use super::accounts::MARK;
use super::{App, Step, Tab};
use crate::account::Harness;
use crate::config::{self, Mode, Relaunch, Strategy};
use crate::view;

/// Name column: the longest key, `close_idle`, and a gap.
const NAME: usize = 13;
/// Value column, before the help when the screen is wide enough.
const VALUE: usize = 18;

#[derive(Default)]
pub struct State {
    /// The selected key, an index into `keys()`.
    pub row: usize,
    /// The text being typed for the selected key.
    pub edit: Option<String>,
}

impl State {
    pub fn editing(&self) -> bool {
        self.edit.is_some()
    }

    pub fn edit_shown(&self, tab: Tab) -> bool {
        tab == Tab::Settings && self.edit.is_some()
    }
}

/// Every key of both harnesses, in the order `hats config` lists them.
pub(super) fn keys() -> Vec<(Harness, &'static str)> {
    Harness::ALL
        .iter()
        .flat_map(|&h| config::keys(h).map(move |(k, _)| (h, *k)))
        .collect()
}

/// The values `←→` steps through, for a key that has a few.
fn choices(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "mode" => Mode::NAMES,
        "strategy" => Strategy::NAMES,
        "relaunch" => Relaunch::NAMES,
        _ => return None,
    })
}

impl App<'_> {
    fn value(&self, harness: Harness, key: &str) -> String {
        self.config
            .get(&format!("{}.{key}", harness.name()))
            .unwrap_or_default()
    }

    pub(super) fn settings_key(&mut self, key: KeyEvent) -> Step {
        let (harness, name) = keys()[self.settings.row];
        if let Some(input) = self.settings.edit.take() {
            return self.edit_key(harness, name, input, key);
        }
        let by: i64 = if key.modifiers.contains(KeyModifiers::SHIFT) {
            5
        } else {
            1
        };
        match key.code {
            KeyCode::Esc => Step::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.move_row(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_row(1),
            KeyCode::Left => self.step(harness, name, -by),
            KeyCode::Right => self.step(harness, name, by),
            KeyCode::Enter => {
                self.settings.edit = Some(self.value(harness, name));
                Step::Redraw
            }
            _ => Step::Idle,
        }
    }

    fn edit_key(&mut self, harness: Harness, name: &str, mut input: String, key: KeyEvent) -> Step {
        match key.code {
            KeyCode::Esc => return Step::Redraw,
            KeyCode::Enter => {
                if !self.save(harness, name, &input) {
                    // Left open with what was typed, to fix it.
                    self.settings.edit = Some(input);
                }
                return Step::Redraw;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.push(c),
            _ => {}
        }
        self.settings.edit = Some(input);
        Step::Redraw
    }

    fn move_row(&mut self, by: isize) -> Step {
        match self
            .settings
            .row
            .checked_add_signed(by)
            .filter(|r| *r < keys().len())
        {
            Some(row) => {
                self.settings.row = row;
                Step::Redraw
            }
            None => Step::Idle,
        }
    }

    /// `←→`: the next choice, a number up or down, or hours of close_idle.
    fn step(&mut self, harness: Harness, name: &str, by: i64) -> Step {
        let current = self.value(harness, name);
        let next = if let Some(choices) = choices(name) {
            let i = choices.iter().position(|c| *c == current).unwrap_or(0) as i64;
            choices[(i + by.signum()).rem_euclid(choices.len() as i64) as usize].to_owned()
        } else if name == "threshold" {
            let n: i64 = current.parse().unwrap_or(90);
            (n + by).clamp(1, 100).to_string()
        } else if name == "close_idle" {
            // Whole hours; from 90m, right is 2h and left 1h.
            let secs = self.config.close_idle.map_or(0, |d| d.as_secs()) as i64;
            let hours = if by > 0 {
                secs / 3600
            } else {
                (secs + 3599) / 3600
            };
            let next = (hours + by).max(0);
            if next == 0 {
                "off".to_owned()
            } else {
                format!("{next}h")
            }
        } else {
            self.info("Press enter to type the labels, separated by commas.".to_owned());
            return Step::Redraw;
        };
        if next == current {
            return Step::Idle;
        }
        self.save(harness, name, &next);
        Step::Redraw
    }

    /// Writes one key and reads everything back; false when it was refused.
    fn save(&mut self, harness: Harness, name: &str, value: &str) -> bool {
        let full = format!("{}.{name}", harness.name());
        let saved = config::set(&self.store.config, &full, value);
        self.reload();
        match saved {
            Ok(()) => {
                let shown = self.value(harness, name);
                self.info(format!("Saved {full} = {shown}."));
                true
            }
            Err(e) => {
                self.fail(format!("Not saved: {e}"));
                false
            }
        }
    }

    // Drawing.

    pub(super) fn draw_settings(
        &self,
        frame: &mut Frame,
        area: Rect,
        message: Rect,
    ) -> Vec<(&'static str, &'static str)> {
        let keys = keys();
        let (harness, name) = keys[self.settings.row];
        let help = format!("{}.{name}: {}", harness.name(), config::help(name));
        let help_lines = view::wrap(&help, usize::from(area.width)).count() as u16;
        let files = self.files();
        let [list, help_area, files_area] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(help_lines + 1),
            Constraint::Length(files.len() as u16),
        ])
        .areas(area);

        // Section titles sit between the keys; the selection counts keys only.
        let wide = usize::from(area.width) >= NAME + VALUE + 80;
        let mut items = Vec::new();
        let mut selected = 0;
        let mut last = None;
        for (i, (h, key)) in keys.iter().enumerate() {
            if last != Some(*h) {
                last = Some(*h);
                items.push(Line::styled(view::title(*h), self.theme.accent()));
            }
            if i == self.settings.row {
                selected = items.len();
            }
            items.push(self.setting(*h, key, i == self.settings.row, wide));
        }
        let widget = List::new(items)
            .highlight_symbol(Line::styled(MARK, self.theme.fg(Color::Cyan)))
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(self.theme.selected());
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(widget, list, &mut state);
        if let Some(input) = &self.settings.edit {
            // The cursor at the end of what is typed, in the value column.
            let row = selected.saturating_sub(state.offset()) as u16;
            let x = (MARK.chars().count() + NAME + input.chars().count()) as u16;
            if row < list.height {
                frame.set_cursor_position(Position::new(
                    list.x + x.min(list.width.saturating_sub(1)),
                    list.y + row,
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(Line::styled(help, self.dim())).wrap(Wrap { trim: false }),
            help_area,
        );
        frame.render_widget(Paragraph::new(files), files_area);
        if self.settings.edit.is_some() {
            self.draw_message(frame, message);
            return vec![("enter", "save"), ("esc", "cancel"), ("⌫", "erase")];
        }
        let change = match name {
            "order" | "disabled" => ("enter", "type labels"),
            _ => ("←→", "change"),
        };
        vec![("↑↓", "setting"), change, ("enter", "type a value")]
    }

    /// One key: its name, its value, and its help when wide enough.
    fn setting(&self, harness: Harness, key: &str, selected: bool, wide: bool) -> Line<'static> {
        let mut spans = vec![Span::raw(format!("{key:<NAME$}"))];
        let editing = selected.then_some(self.settings.edit.as_ref()).flatten();
        let value = match editing {
            Some(input) => {
                spans.push(Span::styled(input.clone(), self.theme.fg(Color::Yellow)));
                input.chars().count()
            }
            None => {
                let mut value = self.value(harness, key);
                match key {
                    "threshold" => value.push_str(" %"),
                    "order" | "disabled" if value.is_empty() => {
                        value = match key {
                            "order" => "by name",
                            _ => "none",
                        }
                        .to_owned()
                    }
                    _ => value = value.replace(',', ", "),
                }
                let shown = if selected && choices(key).is_some() {
                    format!("‹ {value} ›")
                } else {
                    value
                };
                let width = shown.chars().count();
                spans.push(Span::raw(shown));
                width
            }
        };
        if wide && editing.is_none() {
            let help = config::help(key);
            spans.push(Span::raw(" ".repeat(VALUE.saturating_sub(value).max(2))));
            spans.push(Span::styled(help.to_owned(), self.dim()));
        }
        Line::from(spans)
    }

    /// Where hats keeps its files and whether `hats setup` was applied.
    fn files(&self) -> Vec<Line<'static>> {
        let store = self.store;
        let row = |name: &str, value: String| {
            Line::from(vec![
                Span::styled(format!("  {name:<NAME$}"), self.dim()),
                Span::raw(value),
            ])
        };
        let setup = if crate::setup::installed(store) {
            Line::from(vec![
                Span::styled(format!("  {:<NAME$}", "hats setup"), self.dim()),
                Span::styled("applied", self.theme.fg(Color::Green)),
                Span::raw(": hooks and status line switch accounts"),
            ])
        } else {
            Line::from(vec![
                Span::styled(format!("  {:<NAME$}", "hats setup"), self.dim()),
                Span::styled("not applied", self.theme.fg(Color::Yellow)),
                Span::raw(": run `hats setup` so accounts switch by themselves"),
            ])
        };
        vec![
            Line::styled("FILES", self.theme.accent()),
            row("config", super::tilde(store, &store.config)),
            row("accounts", super::tilde(store, &store.data)),
            row("state", super::tilde(store, &store.state)),
            setup,
        ]
    }
}
