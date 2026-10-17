//! The Usage tab: tokens and their cost at API list price today, in 7 and 30
//! days and in the month, by account, model and project, with a bar per day.
//! The scan runs on its own thread; a byte on a pipe wakes the view when it
//! ends, so the screen never waits for it.

use std::collections::HashMap;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, Paragraph};

use super::{App, Step};
use crate::store::Store;
use crate::usage::{self, Day, Report, Row, model_label};
use crate::view;

const PERIODS: usize = 4;
/// Days in the bar chart.
const DAYS: i32 = 30;
/// Projects listed, the most expensive in 30 days first.
const TOP: usize = 8;
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

pub struct State {
    report: Option<Report>,
    error: Option<String>,
    scanning: Option<Receiver<Result<Report, String>>>,
    /// The scan writes a byte when it ends; the view waits on the other end.
    wake: (PipeReader, PipeWriter),
    /// 0 for every account, else an index into `accounts()` plus one.
    filter: usize,
    scroll: u16,
}

impl State {
    pub fn new() -> Self {
        Self {
            report: None,
            error: None,
            scanning: None,
            wake: std::io::pipe().expect("a pipe for the usage scan"),
            filter: 0,
            scroll: 0,
        }
    }

    pub fn wake_fd(&self) -> &PipeReader {
        &self.wake.0
    }

    /// Starts a scan unless one runs.
    pub fn start(&mut self, store: &Store) {
        if self.scanning.is_some() {
            return;
        }
        let Ok(mut wake) = self.wake.1.try_clone() else {
            self.error = Some("could not start the usage scan".to_owned());
            return;
        };
        let store = Store {
            home: store.home.clone(),
            data: store.data.clone(),
            state: store.state.clone(),
            config: store.config.clone(),
        };
        let (done, scanning) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(usage::report(&store, SystemTime::now()));
            let _ = wake.write_all(b"x");
        });
        self.scanning = Some(scanning);
    }

    /// Takes in a scan that ended; true when there is something new to draw.
    pub fn collect(&mut self) -> bool {
        let Some(scanning) = &self.scanning else {
            return false;
        };
        let result = match scanning.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("the usage scan stopped".to_owned()),
        };
        self.scanning = None;
        let mut byte = [0u8; 1];
        let _ = self.wake.0.read(&mut byte);
        match result {
            Ok(report) => {
                self.report = Some(report);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        true
    }

    #[cfg(test)]
    pub fn wait(&mut self) {
        while self.scanning.is_some() && !self.collect() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Cost and tokens in each period: today, 7 days, 30 days, the month.
#[derive(Clone, Copy, Default)]
struct Cells {
    cost: [f64; PERIODS],
    tokens: [u64; PERIODS],
    /// Some of the cost is missing: a model without a price.
    unpriced: [bool; PERIODS],
}

impl Cells {
    fn add(&mut self, period: usize, row: &Row) {
        self.cost[period] += row.cost.unwrap_or(0.0);
        self.tokens[period] += row.tokens.total();
        self.unpriced[period] |= row.cost.is_none();
    }
}

/// The report summed for one account, or for all.
struct Totals {
    total: Cells,
    accounts: Vec<(String, Cells)>,
    models: Vec<(String, Cells)>,
    projects: Vec<(String, Cells)>,
    /// Cost per day, oldest first, the last one today.
    days: Vec<f64>,
}

/// The first day of each period, in `PERIODS` order.
fn starts(today: Day) -> [Day; PERIODS] {
    [
        today,
        today.minus(6),
        today.minus(DAYS - 1),
        today.month_start(),
    ]
}

/// `claude default`, the name of an account in the report.
fn account_name(row: &Row) -> String {
    format!("{} {}", row.harness.name(), row.account_label)
}

/// Every account in the report, the most expensive in 30 days first.
fn accounts(report: &Report) -> Vec<String> {
    let first = report.today.minus(DAYS - 1);
    let mut cost: HashMap<String, f64> = HashMap::new();
    for row in report.rows.iter().filter(|r| r.date >= first) {
        *cost.entry(account_name(row)).or_default() += row.cost.unwrap_or(0.0);
    }
    let mut names: Vec<(String, f64)> = cost.into_iter().collect();
    names.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    names.into_iter().map(|(name, _)| name).collect()
}

fn totals(report: &Report, account: Option<&str>) -> Totals {
    let today = report.today;
    let starts = starts(today);
    let first = *starts.iter().min().expect("four periods");
    let mut total = Cells::default();
    let mut accounts: HashMap<String, Cells> = HashMap::new();
    let mut models: HashMap<String, Cells> = HashMap::new();
    let mut projects: HashMap<&str, (&str, Cells)> = HashMap::new();
    let mut days = vec![0.0; DAYS as usize];
    for row in report.rows.iter().filter(|r| r.date >= first) {
        let name = account_name(row);
        if account.is_some_and(|a| a != name) {
            continue;
        }
        let back = today.0 - row.date.0;
        if (0..DAYS).contains(&back) {
            days[(DAYS - 1 - back) as usize] += row.cost.unwrap_or(0.0);
        }
        // Haiku's long prompts are priced apart and shown with their model.
        let model = model_label(row.model.strip_suffix("+long").unwrap_or(&row.model));
        let by_account = accounts.entry(name).or_default();
        let by_model = models.entry(model).or_default();
        let (_, by_project) = projects
            .entry(&row.project_id)
            .or_insert((&row.project, Cells::default()));
        for (p, start) in starts.iter().enumerate() {
            if row.date >= *start {
                for cells in [
                    &mut total,
                    &mut *by_account,
                    &mut *by_model,
                    &mut *by_project,
                ] {
                    cells.add(p, row);
                }
            }
        }
    }
    let sorted = |mut v: Vec<(String, Cells)>| {
        v.sort_by(|a, b| {
            b.1.cost[2]
                .total_cmp(&a.1.cost[2])
                .then_with(|| a.0.cmp(&b.0))
        });
        v
    };
    Totals {
        total,
        accounts: sorted(accounts.into_iter().collect()),
        models: sorted(models.into_iter().collect()),
        projects: sorted(
            projects
                .into_values()
                .map(|(name, cells)| (name.to_owned(), cells))
                .collect(),
        ),
        days,
    }
}

/// `$1,234.50`, `$0.42`.
fn money(usd: f64) -> String {
    let cents = (usd * 100.0).round() as u64;
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("${grouped}.{:02}", cents % 100)
}

/// `950`, `12.3K`, `4.5M`, `1.2B` tokens.
fn count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}K", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}B", n as f64 / 1e9),
    }
}

impl App<'_> {
    pub(super) fn usage_key(&mut self, key: KeyEvent) -> Step {
        let names = self.usage.report.as_ref().map(accounts).unwrap_or_default();
        let options = names.len() + 1;
        let usage = &mut self.usage;
        match key.code {
            KeyCode::Esc => return Step::Quit,
            KeyCode::Left => usage.filter = (usage.filter + options - 1) % options,
            KeyCode::Right => usage.filter = (usage.filter + 1) % options,
            KeyCode::Up | KeyCode::Char('k') => usage.scroll = usage.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => usage.scroll = usage.scroll.saturating_add(1),
            KeyCode::PageUp => usage.scroll = usage.scroll.saturating_sub(10),
            KeyCode::PageDown => usage.scroll = usage.scroll.saturating_add(10),
            _ => return Step::Idle,
        }
        Step::Redraw
    }

    pub(super) fn draw_usage(
        &self,
        frame: &mut Frame,
        area: Rect,
    ) -> Vec<(&'static str, &'static str)> {
        let keys = vec![("←→", "account"), ("↑↓", "scroll"), ("r", "scan again")];
        let width = usize::from(area.width);
        let note = "What these tokens would cost at API list price, not what a plan charges.";
        let mut head: Vec<Line> = view::wrap(note, width)
            .map(|l| Line::styled(l, self.dim()))
            .collect();
        let Some(report) = &self.usage.report else {
            head.push(Line::default());
            head.push(match &self.usage.error {
                Some(e) => Line::styled(format!("Could not read the logs: {e}"), self.theme.fg(Color::Red)),
                None => Line::raw(if usage::scanned_before(self.store) {
                    "scanning…"
                } else {
                    "scanning… the first time reads every log and takes a few seconds; later ones read only what is new."
                }),
            });
            frame.render_widget(
                Paragraph::new(head).wrap(ratatui::widgets::Wrap { trim: false }),
                area,
            );
            return keys;
        };
        let names = accounts(report);
        let account = self.usage.filter.checked_sub(1).and_then(|i| names.get(i));
        let t = totals(report, account.map(String::as_str));
        let shown = account.map_or("all accounts", String::as_str);
        let mut filter = vec![
            Span::styled("Account ", self.dim()),
            Span::styled(format!("‹ {shown} ›"), self.theme.selected()),
        ];
        let state = match (&self.usage.scanning, &self.usage.error) {
            (Some(_), _) => "  scanning…".to_owned(),
            (None, Some(e)) => format!("  last scan failed: {e}"),
            (None, None) => format!(
                "  scanned {}",
                view::clock(
                    crate::limits::unix_secs(report.scanned_at),
                    SystemTime::now()
                )
            ),
        };
        filter.push(Span::styled(state, self.dim()));
        head.push(Line::from(filter));
        head.push(Line::default());

        let (_, month, _) = report.today.ymd();
        let titles = ["TODAY", "7 DAYS", "30 DAYS", MONTHS[month as usize - 1]];
        let name_width = (width / 4).clamp(14, 28);
        let cell = width.saturating_sub(name_width) / PERIODS;
        let wide = cell >= 20;
        let periods: String = titles
            .iter()
            .map(|t| format!("{:>cell$}", t.to_uppercase()))
            .collect();
        head.push(Line::styled(
            format!("{:name_width$}{periods}", ""),
            self.dim(),
        ));
        head.push(self.cells_line("Total", &t.total, name_width, cell, wide, true));
        head.push(Line::default());

        let chart_height = if area.height >= 30 { 7 } else { 4 };
        let peak = t.days.iter().cloned().fold(0.0, f64::max);
        let [top, chart, axis, rest] = Layout::vertical([
            Constraint::Length(head.len() as u16 + 1),
            Constraint::Length(chart_height),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);
        head.push(Line::from(vec![
            Span::styled(format!("Cost per day, last {DAYS} days"), self.bold()),
            Span::styled(format!("   highest {}", money(peak)), self.dim()),
        ]));
        frame.render_widget(Paragraph::new(head), top);
        let bar_width = ((usize::from(area.width) + 1) / DAYS as usize)
            .saturating_sub(1)
            .clamp(1, 3) as u16;
        let bars: Vec<Bar> = t
            .days
            .iter()
            .map(|cost| {
                Bar::default()
                    .value((cost * 100.0).round() as u64)
                    .text_value(String::new())
            })
            .collect();
        frame.render_widget(
            BarChart::vertical(bars)
                .bar_width(bar_width)
                .bar_gap(1)
                .bar_style(self.theme.fg(Color::Cyan)),
            chart,
        );
        let span = (bar_width as usize + 1) * DAYS as usize - 1;
        let first = report.today.minus(DAYS - 1);
        let (_, fm, fd) = first.ymd();
        let left = format!("{} {fd}", &MONTHS[fm as usize - 1][..3]);
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("{left:<w$}today", w = span.saturating_sub(5)),
                self.dim(),
            )),
            axis,
        );

        let mut lines = vec![Line::default()];
        let section = |title: &str,
                       rows: &[(String, Cells)],
                       limit: usize,
                       lines: &mut Vec<Line<'static>>| {
            lines.push(Line::styled(
                format!("{title:<name_width$}{periods}"),
                self.bold(),
            ));
            for (name, cells) in rows.iter().take(limit) {
                lines.push(self.cells_line(name, cells, name_width, cell, wide, false));
            }
            if rows.len() > limit {
                lines.push(Line::styled(
                    format!("  {} more", rows.len() - limit),
                    self.dim(),
                ));
            }
            lines.push(Line::default());
        };
        if account.is_none() {
            section("BY ACCOUNT", &t.accounts, usize::MAX, &mut lines);
        }
        section("BY MODEL", &t.models, usize::MAX, &mut lines);
        section("BY PROJECT", &t.projects, TOP, &mut lines);
        lines.push(Line::styled(
            format!(
                "List prices checked {}. Logs from {}; hats keeps its own totals from {}.",
                report.prices_checked,
                report.logs_from.map_or("–".to_owned(), |d| d.to_string()),
                report.tracked_from
            ),
            self.dim(),
        ));
        let scroll = self.usage.scroll.min(lines.len().saturating_sub(1) as u16);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .scroll((scroll, 0)),
            rest,
        );
        keys
    }

    /// A name and its four periods: the cost, and the tokens when wide.
    fn cells_line(
        &self,
        name: &str,
        cells: &Cells,
        name_width: usize,
        cell: usize,
        wide: bool,
        strong: bool,
    ) -> Line<'static> {
        let mut text = format!(
            "{:<name_width$}",
            view::fit(name, name_width.saturating_sub(1))
        );
        for p in 0..PERIODS {
            let shown = if cells.tokens[p] == 0 {
                "-".to_owned()
            } else if cells.unpriced[p] && cells.cost[p] == 0.0 {
                format!("no price {}", count(cells.tokens[p]))
            } else if wide {
                format!("{} {:>6}", money(cells.cost[p]), count(cells.tokens[p]))
            } else {
                money(cells.cost[p])
            };
            text.push_str(&format!("{shown:>cell$}"));
        }
        if strong {
            Line::styled(text, self.bold())
        } else {
            Line::raw(text)
        }
    }
}
