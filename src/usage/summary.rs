//! `hats usage`: today, the last 7 and 30 days and the month so far, in
//! total and by account, model and project. Costs fit 80 columns; from 110,
//! tokens sit next to each cost.

use std::collections::HashMap;

use super::{Report, Row, model_label};

const TOP_PROJECTS: usize = 8;
const WIDE: usize = 110;
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

/// Cost and tokens in each period: today, 7 days, 30 days, the month.
#[derive(Clone, Copy, Default)]
struct Cells {
    cost: [f64; 4],
    tokens: [u64; 4],
    /// Some of the cost is missing: a model without a price.
    unpriced: [bool; 4],
}

impl Cells {
    fn add(&mut self, period: usize, row: &Row) {
        self.cost[period] += row.cost.unwrap_or(0.0);
        self.tokens[period] += row.tokens.total();
        self.unpriced[period] |= row.cost.is_none();
    }

    fn merge(&mut self, o: &Cells) {
        for p in 0..4 {
            self.cost[p] += o.cost[p];
            self.tokens[p] += o.tokens[p];
            self.unpriced[p] |= o.unpriced[p];
        }
    }
}

pub fn render(report: &Report, width: usize) -> Vec<String> {
    let today = report.today;
    let (_, month, _) = today.ymd();
    let starts = [today, today.minus(6), today.minus(29), today.month_start()];
    let titles = ["TODAY", "7 DAYS", "30 DAYS", MONTHS[month as usize - 1]];
    let first = *starts.iter().min().expect("four periods");

    let mut total = Cells::default();
    let mut accounts: HashMap<(&str, &str), Cells> = HashMap::new();
    let mut models: HashMap<String, Cells> = HashMap::new();
    let mut projects: HashMap<&str, (&str, Cells)> = HashMap::new();
    let mut unpriced: Vec<&str> = Vec::new();
    for row in report.rows.iter().filter(|r| r.date >= first) {
        // Haiku's long prompts are priced apart and shown with their model.
        let model = model_label(row.model.strip_suffix("+long").unwrap_or(&row.model));
        let account = accounts
            .entry((row.harness.name(), &row.account_label))
            .or_default();
        let by_model = models.entry(model).or_default();
        let (_, by_project) = projects
            .entry(&row.project_id)
            .or_insert((&row.project, Cells::default()));
        for (p, start) in starts.iter().enumerate() {
            if row.date >= *start {
                for cells in [&mut total, &mut *account, &mut *by_model, &mut *by_project] {
                    cells.add(p, row);
                }
            }
        }
        if row.cost.is_none() && !unpriced.contains(&row.model.as_str()) {
            unpriced.push(&row.model);
        }
    }

    let mut out = Vec::new();
    let note = "USAGE · what these tokens cost at API list price, not what a plan charges";
    out.extend(crate::view::wrap(note, width));
    out.push(String::new());
    if total.tokens.iter().all(|t| *t == 0) {
        out.push("No use in the agents' logs in the last 30 days.".to_owned());
        out.extend(footer(report, &unpriced, width));
        return out;
    }

    let wide = width >= WIDE;
    let cell = if wide { 19 } else { 11 };
    let name_width = width.saturating_sub(4 * cell).max(16);
    let line = |name: &str, cells: &Cells| -> String {
        let mut s = fit(name, name_width);
        for p in 0..4 {
            let cost = if cells.unpriced[p] && cells.cost[p] == 0.0 {
                "no price".to_owned()
            } else {
                money(cells.cost[p])
            };
            if wide {
                s.push_str(&format!("{cost:>11} {:>7}", count(cells.tokens[p])));
            } else {
                s.push_str(&format!("{cost:>11}"));
            }
        }
        s.trim_end().to_owned()
    };

    let mut head = " ".repeat(name_width);
    for title in titles {
        let title = title.to_uppercase();
        if wide {
            head.push_str(&format!("{title:>19}"));
        } else {
            head.push_str(&format!("{title:>11}"));
        }
    }
    out.push(head);
    out.push(line("Total", &total));
    if !wide {
        let mut tokens = fit("  tokens", name_width);
        for t in total.tokens {
            tokens.push_str(&format!("{:>11}", count(t)));
        }
        out.push(tokens);
    }

    let sorted = |cells: Vec<(String, Cells)>| {
        let mut cells: Vec<_> = cells
            .into_iter()
            .filter(|(_, c)| c.tokens.iter().any(|t| *t > 0))
            .collect();
        cells.sort_by(|a, b| {
            b.1.cost[2]
                .total_cmp(&a.1.cost[2])
                .then(b.1.tokens[2].cmp(&a.1.tokens[2]))
                .then(a.0.cmp(&b.0))
        });
        cells
    };
    out.push(String::new());
    out.push("ACCOUNT".to_owned());
    let accounts = accounts
        .into_iter()
        .map(|((harness, label), c)| (format!("{harness:<6} {label}"), c))
        .collect();
    for (name, cells) in sorted(accounts) {
        out.push(line(&format!("  {name}"), &cells));
    }
    out.push("MODEL".to_owned());
    for (name, cells) in sorted(models.into_iter().collect()) {
        out.push(line(&format!("  {name}"), &cells));
    }
    out.push("PROJECT".to_owned());
    let projects = sorted(
        projects
            .into_values()
            .map(|(label, c)| (label.to_owned(), c))
            .collect(),
    );
    for (name, cells) in projects.iter().take(TOP_PROJECTS) {
        out.push(line(&format!("  {name}"), cells));
    }
    if projects.len() > TOP_PROJECTS {
        let mut rest = Cells::default();
        for (_, cells) in &projects[TOP_PROJECTS..] {
            rest.merge(cells);
        }
        let name = format!("  {} more", projects.len() - TOP_PROJECTS);
        out.push(line(&name, &rest));
    }
    out.push(String::new());
    out.extend(footer(report, &unpriced, width));
    out
}

fn footer(report: &Report, unpriced: &[&str], width: usize) -> Vec<String> {
    let mut text = match report.logs_from {
        Some(from) => format!(
            "Logs from {from}; hats keeps what it counted after Claude Code deletes a \
             transcript (30 days). "
        ),
        None => String::new(),
    };
    text.push_str(&format!("Prices of {}.", report.prices_checked));
    if !unpriced.is_empty() {
        text.push_str(&format!(
            " No list price for {}: counted in tokens only.",
            unpriced.join(", ")
        ));
    }
    crate::view::wrap(&text, width).collect()
}

fn fit(text: &str, width: usize) -> String {
    let mut s: String = text.chars().take(width.saturating_sub(1)).collect();
    if text.chars().count() >= width {
        s.pop();
        s.push('…');
    }
    format!("{s:<width$}")
}

/// `$0.42`, `$123.45`, `$1,234`.
fn money(usd: f64) -> String {
    if usd < 1000.0 {
        return format!("${usd:.2}");
    }
    let whole = format!("{:.0}", usd);
    let mut out = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("${out}")
}

/// `950`, `12.3K`, `4.5M`, `1.2B`.
fn count(n: u64) -> String {
    let n = n as f64;
    match n {
        n if n >= 1e9 => format!("{:.1}B", n / 1e9),
        n if n >= 1e6 => format!("{:.1}M", n / 1e6),
        n if n >= 1e3 => format!("{:.1}K", n / 1e3),
        n => format!("{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Harness;
    use crate::usage::{Day, PRICES_CHECKED, Stats, Tokens};

    fn row(date: &str, label: &str, model: &str, cost: Option<f64>) -> Row {
        Row {
            date: Day::parse(date).unwrap(),
            harness: Harness::Claude,
            account: format!("claude:{label}"),
            account_label: label.to_owned(),
            model: model.to_owned(),
            project: "demo".to_owned(),
            project_id: "demo~abcdef".to_owned(),
            tokens: Tokens {
                requests: 1,
                input: 1000,
                ..Tokens::default()
            },
            cost,
        }
    }

    #[test]
    fn periods_end_today_and_the_month_starts_on_its_first_local_day() {
        let report = Report {
            scanned_at: std::time::UNIX_EPOCH,
            today: Day::parse("2026-02-03").unwrap(),
            logs_from: Day::parse("2025-12-01"),
            tracked_from: Day::parse("2026-02-03").unwrap(),
            prices_checked: PRICES_CHECKED,
            rows: vec![
                row("2025-12-01", "old", "claude-opus-5-5", Some(100.0)),
                row("2026-01-31", "work", "claude-opus-5-5", Some(10.0)),
                row("2026-02-01", "work", "claude-opus-5-5+fast", Some(1.0)),
                row("2026-02-03", "side", "claude-haiku-5-5+long", Some(0.5)),
                row("2026-02-03", "side", "mystery", None),
            ],
            scan: Stats::default(),
        };
        let text = render(&report, 80);
        let find = |start: &str| {
            text.iter()
                .find(|l| l.starts_with(start))
                .unwrap_or_else(|| panic!("{start}: {text:#?}"))
                .split_whitespace()
                .skip(start.split_whitespace().count())
                .collect::<Vec<_>>()
        };
        assert_eq!(find(" ").last(), Some(&"FEBRUARY"));
        assert_eq!(find("Total"), ["$0.50", "$11.50", "$11.50", "$1.50"]);
        assert_eq!(find("  tokens"), ["2.0K", "4.0K", "4.0K", "3.0K"]);
        assert_eq!(
            find("  claude work"),
            ["$0.00", "$11.00", "$11.00", "$1.00"]
        );
        // A long prompt is its model's use; fast mode is apart.
        assert_eq!(find("  Haiku 5.5"), ["$0.50", "$0.50", "$0.50", "$0.50"]);
        assert_eq!(
            find("  Opus 5.5 · fast"),
            ["$0.00", "$1.00", "$1.00", "$1.00"]
        );
        assert_eq!(find("  mystery"), ["no", "price"].repeat(4));
        assert!(!text.iter().any(|l| l.contains("old")));
        assert!(text.iter().any(|l| l.contains("No list price for mystery")));
    }

    #[test]
    fn amounts() {
        assert_eq!(money(0.0), "$0.00");
        assert_eq!(money(123.456), "$123.46");
        assert_eq!(money(1234.5), "$1,234");
        assert_eq!(money(1_234_567.0), "$1,234,567");
        assert_eq!(count(950), "950");
        assert_eq!(count(12_345), "12.3K");
        assert_eq!(count(4_500_000), "4.5M");
        assert_eq!(count(1_230_000_000), "1.2B");
    }
}
