use std::fs;
use std::time::SystemTime;

use ratatui::backend::TestBackend;

use super::accounts::Prompt;
use super::*;
use crate::account::Account;
use crate::limits::{self, LimitWindow, unix_secs};

/// A terminal with a dark background, as the view sees it once asked.
const DARK: Theme = Theme {
    color: true,
    selection: Some(Color::Rgb(62, 64, 69)),
};

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

fn keys(app: &mut App, text: &str) {
    for c in text.chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
}

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|frame| app.draw(frame)).unwrap();
    term
}

fn text(app: &mut App) -> String {
    text_at(app, 100, 30)
}

fn text_at(app: &mut App, width: u16, height: u16) -> String {
    let term = render(app, width, height);
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

fn config(store: &Store) -> Config {
    Config::load(&store.config).unwrap()
}

#[test]
fn tabs_change_with_tab_and_digits_and_the_foot_shows_their_keys() {
    let (_dir, store) = sandbox();
    account(&store, "work", Some(42.0));
    let mut app = App::new(&store, DARK);
    let screen = text(&mut app);
    assert!(screen.starts_with(" 1 Accounts  2 Usage  3 Settings  4 Activity"));
    assert!(screen.lines().last().unwrap().contains("space off"));

    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Usage);
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(app.tab, Tab::Accounts);
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(app.tab, Tab::Activity);
    app.on_key(key(KeyCode::Char('3')));
    assert_eq!(app.tab, Tab::Settings);
    let foot = text(&mut app).lines().last().unwrap().to_owned();
    assert!(
        foot.contains("setting") && !foot.contains("login"),
        "{foot}"
    );
    assert_eq!(app.on_key(key(KeyCode::Char('3'))), Step::Idle);
    assert_eq!(app.on_key(key(KeyCode::Esc)), Step::Quit);
    assert_eq!(app.closed_by, Some("Esc"));
}

#[test]
fn accounts_show_one_line_each_and_the_selected_ones_details() {
    let (_dir, store) = sandbox();
    let work = account(&store, "work", Some(42.0));
    account(&store, "personal", None);
    let id = work.provider_id(&store).unwrap().unwrap();
    auto::choose(&store, &work, &id, SystemTime::now()).unwrap();

    let mut app = App::new(&store, DARK);
    let screen = text(&mut app);
    assert!(
        line_with(&screen, "Claude Code 3").contains("new sessions start in ● work"),
        "{screen}"
    );
    assert!(screen.contains("Auto: at 90 % of either limit"));
    let row = line_with(&screen, "▌ ● work");
    for part in ["▰▰", " 42% ", " 12% ", "just now"] {
        assert!(row.contains(part), "{part:?} in {row}");
    }
    assert!(line_with(&screen, "personal").contains("not signed in: press l to sign in"));
    // The details of the selected account, under the list.
    assert!(line_with(&screen, "Mail").contains("work@example.com"));
    assert!(line_with(&screen, "Organization").contains("Acme"));
    assert!(line_with(&screen, "Resets").contains("5-hour 42 % until"));
    assert!(line_with(&screen, "Last change").contains("default → work: you chose it"));
    assert!(!screen.contains("CODEX"), "one harness at a time");

    app.on_key(key(KeyCode::Right));
    let screen = text(&mut app);
    assert!(screen.contains("No codex accounts yet"), "{screen}");
    assert!(screen.lines().last().unwrap().contains("←→ claude"));
}

#[test]
fn the_selected_row_keeps_its_colors_on_a_background() {
    let (_dir, store) = sandbox();
    account(&store, "work", Some(95.0));
    account(&store, "side", Some(10.0));
    let mut app = App::new(&store, DARK);
    app.select_label(Some("work"));
    // Wide, so who it is has its column, dim on the other rows.
    let term = render(&mut app, 140, 30);
    let buffer = term.backend().buffer();
    let y = (0..30)
        .find(|&y| {
            (0..140)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains(" work ")
        })
        .unwrap();
    let cells: Vec<_> = (0..140).map(|x| &buffer[(x, y)]).collect();
    assert!(
        cells.iter().all(|c| !c.modifier.contains(Modifier::DIM)),
        "nothing dim on the selected row"
    );
    assert!(
        cells.iter().all(|c| c.bg == Color::Rgb(62, 64, 69)),
        "the whole row"
    );
    assert!(
        cells
            .iter()
            .all(|c| !c.modifier.contains(Modifier::REVERSED))
    );
    assert!(cells.iter().all(|c| c.modifier.contains(Modifier::BOLD)));
    assert!(
        cells.iter().any(|c| c.fg == Color::Red),
        "the bar past 90 % stays red"
    );
    assert_eq!(cells[0].fg, Color::Cyan, "the mark");

    // A terminal that did not say its background: mark and bold only.
    let mut app = App::new(
        &store,
        Theme {
            selection: None,
            ..DARK
        },
    );
    let term = render(&mut app, 100, 30);
    assert!(
        term.backend()
            .buffer()
            .content()
            .iter()
            .all(|c| c.bg == Color::Reset)
    );
}

#[test]
fn space_turns_an_account_off_and_on_and_enter_still_uses_it() {
    let (_dir, store) = sandbox();
    account(&store, "work", Some(10.0));
    account(&store, "side", Some(10.0));
    let mut app = App::new(&store, DARK);
    app.select_label(Some("side"));

    app.on_key(key(KeyCode::Char(' ')));
    assert_eq!(config(&store).claude.disabled, ["side"]);
    let screen = text(&mut app);
    assert!(
        line_with(&screen, "▌   side").contains("side off"),
        "{screen}"
    );
    assert!(line_with(&screen, "Auto switch").contains("off: never moves here"));
    assert!(screen.lines().last().unwrap().contains("space on"));

    // Every other account off: auto mode says it cannot move.
    app.select_label(Some("work"));
    app.on_key(key(KeyCode::Char(' ')));
    app.select_label(Some("default"));
    assert!(
        text(&mut app).contains("Nothing to move to"),
        "{}",
        text(&mut app)
    );

    app.select_label(Some("side"));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(store.current(Harness::Claude).unwrap().label, "side");
    let message = app.message.as_ref().unwrap();
    assert!(
        message.error && message.text.contains("'side' is off"),
        "{}",
        message.text
    );

    app.on_key(key(KeyCode::Char(' ')));
    assert_eq!(config(&store).claude.disabled, ["work"]);
}

#[test]
fn shift_arrows_reorder_and_save_the_order() {
    let (_dir, store) = sandbox();
    for label in ["a", "x", "y"] {
        account(&store, label, None);
    }
    let mut app = App::new(&store, DARK);
    let labels = |app: &App| -> Vec<String> {
        app.groups[0]
            .statuses
            .iter()
            .map(|s| s.account.label.clone())
            .collect()
    };
    // The harness's own directory exists, so `default` is listed by name.
    assert_eq!(labels(&app), ["a", "default", "x", "y"]);
    app.select_label(Some("x"));
    app.on_key(shift(KeyCode::Up));
    assert_eq!(config(&store).claude.order, ["a", "x", "default", "y"]);
    assert_eq!(labels(&app), ["a", "x", "default", "y"]);
    assert_eq!(app.selected_status().unwrap().account.label, "x");
    app.on_key(key(KeyCode::Char('J')));
    assert_eq!(config(&store).claude.order, ["a", "default", "x", "y"]);
    app.on_key(shift(KeyCode::Down));
    assert_eq!(config(&store).claude.order, ["a", "default", "y", "x"]);
    app.on_key(shift(KeyCode::Down));
    assert_eq!(
        config(&store).claude.order,
        ["a", "default", "y", "x"],
        "already last"
    );
}

#[test]
fn the_cli_actions_and_their_prompts() {
    let (_dir, store) = sandbox();
    account(&store, "work", Some(10.0));
    account(&store, "personal", None);
    let mut app = App::new(&store, DARK);
    let child = |action, label: &str| {
        Step::Run(Child {
            action,
            harness: Harness::Claude,
            label: label.to_owned(),
        })
    };
    app.select_label(Some("personal"));
    app.on_key(key(KeyCode::Enter));
    assert!(app.message.as_ref().unwrap().text.contains("not signed in"));
    app.select_label(Some("work"));
    assert_eq!(
        app.on_key(key(KeyCode::Char('l'))),
        child(Action::Login, "work")
    );
    app.on_key(key(KeyCode::Char('d')));
    assert_eq!(app.accounts.prompt, Some(Prompt::Remove("work".to_owned())));
    assert!(text(&mut app).contains("Remove 'work'? It signs out"));
    assert_eq!(app.on_key(key(KeyCode::Char('n'))), Step::Redraw);
    app.on_key(key(KeyCode::Char('d')));
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        child(Action::Remove, "work")
    );
    app.select_label(Some("default"));
    app.on_key(key(KeyCode::Char('d')));
    assert_eq!(app.accounts.prompt, None, "default is never removed");

    app.on_key(key(KeyCode::Char('a')));
    // A prompt takes every key: digits and q do not change tab or quit.
    keys(&mut app, "q2x");
    app.on_key(key(KeyCode::Backspace));
    assert!(text(&mut app).contains("New claude account label: q2"));
    assert_eq!(app.tab, Tab::Accounts);
    assert_eq!(app.on_key(key(KeyCode::Enter)), child(Action::Add, "q2"));
}

/// Selects the row of `key` in the Settings tab.
fn setting(app: &mut App, harness: Harness, name: &str) {
    app.show(Tab::Settings);
    let all = super::settings::keys();
    app.settings.row = all.iter().position(|k| *k == (harness, name)).unwrap();
}

#[test]
fn settings_change_in_place_and_are_saved_at_once() {
    let (_dir, store) = sandbox();
    account(&store, "work", None);
    let mut app = App::new(&store, DARK);
    let screen = {
        app.on_key(key(KeyCode::Char('3')));
        text_at(&mut app, 140, 40)
    };
    assert!(line_with(&screen, "mode").contains("‹ auto ›"), "{screen}");
    assert!(line_with(&screen, "close_idle").contains("32h"));
    assert!(
        line_with(&screen, "close_idle").contains("close a `hats run` session unused"),
        "each with its help when wide: {screen}"
    );
    assert!(screen.contains("CODEX"));
    assert!(line_with(&screen, "config").contains("config/hats/config.toml"));
    assert!(line_with(&screen, "hats setup").contains("not applied"));

    app.on_key(key(KeyCode::Right));
    assert_eq!(config(&store).claude.mode, crate::config::Mode::Manual);
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .text
            .contains("Saved claude.mode = manual")
    );

    setting(&mut app, Harness::Claude, "threshold");
    app.on_key(key(KeyCode::Right));
    assert_eq!(config(&store).claude.threshold, 91);
    app.on_key(shift(KeyCode::Left));
    assert_eq!(config(&store).claude.threshold, 86);
    // Enter opens the value to type; enter again saves it.
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Backspace));
    app.on_key(key(KeyCode::Backspace));
    keys(&mut app, "75");
    assert_eq!(app.settings.edit.as_deref(), Some("75"));
    assert!(line_with(&text(&mut app), "threshold").contains("75"));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(config(&store).claude.threshold, 75);
    assert!(text(&mut app).contains("Auto: at 75 %") || app.tab == Tab::Settings);

    setting(&mut app, Harness::Claude, "close_idle");
    app.on_key(key(KeyCode::Right));
    assert_eq!(config(&store).close_idle.unwrap().as_secs(), 33 * 3600);
    app.on_key(key(KeyCode::Enter));
    for _ in 0..3 {
        app.on_key(key(KeyCode::Backspace));
    }
    keys(&mut app, "90m");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(config(&store).close_idle.unwrap().as_secs(), 90 * 60);
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .text
            .contains("close_idle = 90m")
    );

    setting(&mut app, Harness::Codex, "strategy");
    app.on_key(key(KeyCode::Left));
    assert_eq!(config(&store).codex.strategy, crate::config::Strategy::Next);
    assert_eq!(
        config(&store).claude.strategy,
        crate::config::Strategy::Headroom
    );
    let text = fs::read_to_string(&store.config).unwrap();
    assert!(text.contains("# Percent of the 5-hour"), "comments kept");
}

#[test]
fn digits_change_tab_even_on_a_number_and_enter_opens_its_edit() {
    let (_dir, store) = sandbox();
    account(&store, "work", None);
    let mut app = App::new(&store, DARK);
    for name in ["threshold", "close_idle"] {
        setting(&mut app, Harness::Claude, name);
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.tab, Tab::Activity, "on {name}");
        assert_eq!(app.settings.edit, None, "on {name}");
    }
    assert_eq!(
        config(&store),
        Config::default(),
        "nothing typed into a value"
    );

    // In the edit that enter opens, digits are the value.
    setting(&mut app, Harness::Claude, "threshold");
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Backspace));
    app.on_key(key(KeyCode::Backspace));
    keys(&mut app, "42");
    assert_eq!(app.tab, Tab::Settings);
    app.on_key(key(KeyCode::Enter));
    assert_eq!(config(&store).claude.threshold, 42);
}

#[test]
fn an_invalid_value_is_refused_with_its_reason_and_left_to_fix() {
    let (_dir, store) = sandbox();
    account(&store, "work", None);
    let mut app = App::new(&store, DARK);
    setting(&mut app, Harness::Claude, "threshold");
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Backspace));
    app.on_key(key(KeyCode::Backspace));
    keys(&mut app, "150");
    app.on_key(key(KeyCode::Enter));
    let message = app.message.as_ref().unwrap();
    assert!(message.error, "{}", message.text);
    assert!(
        message.text.contains("from 1 to 100, got '150'"),
        "{}",
        message.text
    );
    assert_eq!(
        app.settings.edit.as_deref(),
        Some("150"),
        "still open to fix"
    );
    assert!(text(&mut app).contains("Not saved: threshold must be"));
    assert!(!store.config.exists(), "nothing written");

    // Esc leaves the old value.
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.settings.edit, None);
    assert_eq!(app.tab, Tab::Settings);

    setting(&mut app, Harness::Claude, "close_idle");
    app.on_key(key(KeyCode::Enter));
    keys(&mut app, "x");
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .text
            .contains("close_idle must be")
    );
    app.on_key(key(KeyCode::Esc));

    setting(&mut app, Harness::Claude, "disabled");
    app.on_key(key(KeyCode::Enter));
    keys(&mut app, "../x");
    app.on_key(key(KeyCode::Enter));
    assert!(app.message.as_ref().unwrap().text.contains("invalid label"));
    assert_eq!(config(&store), Config::default());
}

#[test]
fn usage_scans_on_its_own_thread_and_sums_per_account() {
    let (_dir, store) = sandbox();
    let work = account(&store, "work", Some(10.0));
    let id = work.provider_id(&store).unwrap().unwrap();
    // A session of 'work' today with one priced request.
    let now = SystemTime::now();
    let at = registry::utc_millis(now);
    store.ensure_state().unwrap();
    fs::write(
        store.registry(),
        format!(
            r#"{{"v":1,"at":"{at}","harness":"claude","account":"{id}","label":"work","configDir":"{}","session":"s1"}}"#,
            work.dir.display()
        ) + "\n",
    )
    .unwrap();
    let project = store.home.join(".claude/projects/-src-demo");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("s1.jsonl"),
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"message":{{"model":"claude-opus-5-5","id":"m1","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{{"input_tokens":1000000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}},"requestId":"r1","type":"assistant","uuid":"u1","timestamp":"{at}","cwd":"/src/demo","sessionId":"s1","version":"2.1.0"}}"#
        ) + "\n",
    )
    .unwrap();

    let mut app = App::new(&store, DARK);
    app.on_key(key(KeyCode::Char('2')));
    assert!(text(&mut app).contains("scanning…"));
    app.usage.wait();
    let screen = text_at(&mut app, 140, 40);
    assert!(screen.contains("not what a plan charges"), "{screen}");
    // One million input tokens of Opus 5.5 at $4.
    assert!(line_with(&screen, "Total").contains("$4.00"), "{screen}");
    assert!(line_with(&screen, "claude work").contains("$4.00"));
    assert!(line_with(&screen, "Opus 5.5").contains("$4.00"));
    assert!(line_with(&screen, "demo").contains("$4.00"));

    app.on_key(key(KeyCode::Right));
    let screen = text_at(&mut app, 140, 40);
    assert!(screen.contains("‹ claude work ›"), "{screen}");
    assert!(!screen.contains("BY ACCOUNT"), "one account needs no split");
}

#[test]
fn activity_lists_changes_and_closes_newest_first() {
    let (_dir, store) = sandbox();
    let work = account(&store, "work", Some(10.0));
    let id = work.provider_id(&store).unwrap().unwrap();
    auto::choose(
        &store,
        &work,
        &id,
        SystemTime::now() - Duration::from_secs(600),
    )
    .unwrap();
    let close = format!(
        r#"{{"v":1,"at":"{}","harness":"claude","session":"s","label":"work","idle":"32h","cwd":"/src/app"}}"#,
        registry::utc_millis(SystemTime::now())
    );
    fs::write(store.state.join("closes.jsonl"), close + "\nnot json\n").unwrap();
    let mut app = App::new(&store, DARK);
    app.on_key(key(KeyCode::Char('4')));
    let screen = text(&mut app);
    let lines: Vec<&str> = screen.lines().skip(2).take(2).collect();
    assert!(
        lines[0].contains("closed a session on work after 32h unused · app"),
        "{screen}"
    );
    assert!(
        lines[1].contains("default → work: you chose it"),
        "{screen}"
    );
}

#[test]
fn every_tab_fits_80_columns_and_24_rows() {
    let (_dir, store) = sandbox();
    account(&store, "work", Some(42.0));
    account(&store, "personal", None);
    let mut app = App::new(&store, DARK);
    for tab in ["1", "2", "3", "4"] {
        keys(&mut app, tab);
        let screen = text_at(&mut app, 80, 24);
        let foot = screen.lines().last().unwrap();
        // The foot is whole: its last key is not cut.
        assert!(
            ["remove", "scan again", "type a value", "newest/oldest"]
                .iter()
                .any(|end| foot.ends_with(end)),
            "{foot}"
        );
    }
}

#[test]
fn no_color_draws_without_colors() {
    let (_dir, store) = sandbox();
    account(&store, "work", Some(95.0));
    let colors = |theme: Theme| {
        let mut app = App::new(&store, theme);
        let term = render(&mut app, 100, 30);
        term.backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.fg != Color::Reset || cell.bg != Color::Reset)
            .count()
    };
    assert!(colors(DARK) > 0);
    assert_eq!(colors(Theme::PLAIN), 0);
}

#[test]
fn says_how_many_claude_code_sessions_run_without_hats() {
    let (_dir, store) = sandbox();
    account(&store, "work", Some(42.0));
    account(&store, "side", Some(10.0));
    let mut app = App::new(&store, DARK);
    assert!(
        !app.count_sessions,
        "tests never read the computer's processes"
    );
    assert!(!text(&mut app).contains("without hats"));

    app.unsupervised = 2;
    let screen = text(&mut app);
    let line =
        "2 Claude Code sessions run without hats and never move: reopen them in a new terminal.";
    assert!(screen.contains(line), "{screen}");
    let below = screen
        .lines()
        .skip_while(|l| !l.contains(line))
        .nth(1)
        .unwrap();
    assert!(
        below.contains("5-HOUR LIMIT"),
        "above the column titles: {screen}"
    );
    app.unsupervised = 1;
    assert!(text(&mut app).contains("1 Claude Code session runs without hats"));
    app.on_key(key(KeyCode::Right));
    assert!(!text(&mut app).contains("without hats"), "not under Codex");
    app.on_key(key(KeyCode::Left));

    app.select_label(Some("side"));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.message.as_ref().unwrap().text,
        "New claude sessions use 'side'."
    );
}
