//! The Activity tab: the changes of account in the registry and the
//! sessions closed for going unused, newest first, in words.
//!
//! A close is one line of `<state>/closes.jsonl`:
//!
//! ```json
//! {"v":1,"at":"2026-01-02T03:04:05.678Z","harness":"claude","session":"<id>","label":"work","idle":"32h","cwd":"/src/app"}
//! ```

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListState, Paragraph};
use serde::Deserialize;

use super::accounts::MARK;
use super::{App, Step};
use crate::account::Harness;
use crate::registry::{self, Change};
use crate::store::Store;
use crate::view;

pub const CLOSES: &str = "closes.jsonl";
/// Closes read from the end of the file.
const TAIL: u64 = 64 * 1024;

#[derive(Default)]
pub struct State {
    events: Vec<Event>,
    selected: usize,
}

/// Something that happened, at Unix seconds `at`.
#[derive(Debug, PartialEq)]
pub struct Event {
    pub at: i64,
    pub harness: Harness,
    pub what: What,
}

#[derive(Debug, PartialEq)]
pub enum What {
    Change(Change),
    Close(Close),
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct Close {
    at: String,
    harness: Harness,
    label: String,
    idle: String,
    #[serde(default)]
    cwd: Option<String>,
}

impl State {
    /// Takes the changes already read and reads the closes again.
    pub fn reload(&mut self, store: &Store, changes: &[Change]) {
        let mut events: Vec<Event> = changes
            .iter()
            .map(|c| Event {
                at: c.at,
                harness: c.harness,
                what: What::Change(c.clone()),
            })
            .collect();
        events.extend(
            closes(&store.state.join(CLOSES))
                .into_iter()
                .filter_map(|c| {
                    Some(Event {
                        at: registry::parse_utc(&c.at)?,
                        harness: c.harness,
                        what: What::Close(c),
                    })
                }),
        );
        events.sort_by_key(|e| std::cmp::Reverse(e.at));
        self.selected = self.selected.min(events.len().saturating_sub(1));
        self.events = events;
    }
}

/// The closes in the file's tail, oldest first; a missing file has none.
fn closes(path: &Path) -> Vec<Close> {
    let mut tail = Vec::new();
    let read = File::open(path).and_then(|mut file| {
        let start = file.metadata()?.len().saturating_sub(TAIL);
        file.seek(SeekFrom::Start(start))?;
        file.read_to_end(&mut tail)?;
        Ok(start)
    });
    let Ok(start) = read else {
        return Vec::new();
    };
    String::from_utf8_lossy(&tail)
        .lines()
        // A tail that starts mid-file starts mid-line.
        .skip(usize::from(start > 0))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

impl App<'_> {
    pub(super) fn activity_key(&mut self, key: KeyEvent) -> Step {
        let len = self.activity.events.len();
        let state = &mut self.activity;
        let next = match key.code {
            KeyCode::Esc => return Step::Quit,
            KeyCode::Up | KeyCode::Char('k') => state.selected.checked_sub(1),
            KeyCode::Down | KeyCode::Char('j') => Some(state.selected + 1),
            KeyCode::PageUp => Some(state.selected.saturating_sub(10)),
            KeyCode::PageDown => Some(state.selected + 10),
            KeyCode::Home => Some(0),
            KeyCode::End => len.checked_sub(1),
            _ => return Step::Idle,
        };
        match next.map(|i| i.min(len.saturating_sub(1))) {
            Some(i) if i != state.selected => {
                state.selected = i;
                Step::Redraw
            }
            _ => Step::Idle,
        }
    }

    pub(super) fn draw_activity(
        &self,
        frame: &mut Frame,
        area: Rect,
    ) -> Vec<(&'static str, &'static str)> {
        let keys = vec![("↑↓", "scroll"), ("home/end", "newest/oldest")];
        if self.activity.events.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "Nothing yet: changes of account and sessions closed for going unused show here.",
                    self.dim(),
                )),
                area,
            );
            return keys;
        }
        let now = SystemTime::now();
        let items: Vec<Line> = self
            .activity
            .events
            .iter()
            .map(|e| self.event(e, now))
            .collect();
        let widget = List::new(items)
            .highlight_symbol(Line::styled(MARK, self.theme.fg(Color::Cyan)))
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(self.theme.selected());
        let mut state = ListState::default().with_selected(Some(self.activity.selected));
        frame.render_stateful_widget(widget, area, &mut state);
        keys
    }

    /// `Fri 14:22  Claude  → default: you chose it`.
    fn event(&self, e: &Event, now: SystemTime) -> Line<'static> {
        let harness = match e.harness {
            Harness::Claude => "Claude",
            Harness::Codex => "Codex ",
        };
        let mut spans = vec![
            Span::styled(format!("{:>9}  ", view::clock(e.at, now)), self.dim()),
            Span::styled(format!("{harness}  "), self.dim()),
        ];
        match &e.what {
            What::Change(c) => spans.push(Span::raw(view::what_changed(c))),
            What::Close(c) => {
                spans.push(Span::styled(
                    format!("closed a session on {} after {} unused", c.label, c.idle),
                    self.theme.fg(Color::Yellow),
                ));
                if let Some(cwd) = &c.cwd {
                    let folder = Path::new(cwd)
                        .file_name()
                        .map_or(cwd.clone(), |f| f.to_string_lossy().into_owned());
                    spans.push(Span::styled(format!(" · {folder}"), self.dim()));
                }
            }
        }
        Line::from(spans)
    }
}
