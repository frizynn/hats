//! `hats usage`: today, the last 7 and 30 days and the month so far, in
//! total and by account, model and project. Costs fit 80 columns; from 110,
//! tokens sit next to each cost.

use super::{Report, Row};

pub fn render(_report: &Report, _width: usize) -> Vec<String> {
    Vec::new()
}

fn money(_usd: f64) -> String {
    String::new()
}

fn count(_n: u64) -> String {
    String::new()
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
