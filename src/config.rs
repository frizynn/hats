//! `config.toml`: how each harness switches accounts. Read with
//! validation that names the key and the allowed values; edited in place by
//! `hats config set`, keeping the user's comments.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use toml_edit::{Array, DocumentMut, Item, Value, value};

use crate::account::Harness;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Switch the default account by itself when the current one passes the
    /// threshold, and move running `hats run` sessions when they are idle.
    Auto,
    /// Only `hats use` changes the account.
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// The account with the most limit left.
    Headroom,
    /// The next account in `order` that is below the threshold.
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relaunch {
    /// Relaunch a running `hats run` session on the new account when the
    /// agent is idle, resuming the same conversation.
    Idle,
    /// Leave running sessions on their account.
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessConfig {
    pub mode: Mode,
    pub threshold: u8,
    pub strategy: Strategy,
    pub order: Vec<String>,
    pub relaunch: Relaunch,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            threshold: 90,
            strategy: Strategy::Headroom,
            order: Vec::new(),
            relaunch: Relaunch::Idle,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub claude: HarnessConfig,
    pub codex: HarnessConfig,
    /// `claude.close_idle`: how long a `hats run` session may go without use
    /// before it is closed; `None` never. Claude Code only: a Codex session
    /// has no hats parent to close it.
    pub close_idle: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            claude: HarnessConfig::default(),
            codex: HarnessConfig::default(),
            close_idle: Some(Duration::from_secs(32 * 3600)),
        }
    }
}

/// What `hats config` shows and accepts for both harnesses, with one line of
/// help each.
pub const KEYS: &[(&str, &str)] = &[
    (
        "mode",
        "auto | manual: switch by itself at the threshold, or only with `hats use`",
    ),
    (
        "threshold",
        "1-100: percent of the 5-hour or weekly limit that triggers a switch",
    ),
    (
        "strategy",
        "headroom | next: most limit left, or next in `order` below the threshold",
    ),
    (
        "order",
        "account labels in preferred order, comma separated (others follow by name)",
    ),
    (
        "relaunch",
        "idle | off: move running `hats run` sessions when the agent is idle",
    ),
];

/// Keys only Claude Code has: they act through the `hats run` parent.
const CLAUDE_KEYS: &[(&str, &str)] = &[(
    "close_idle",
    "close a `hats run` session unused for this long (32h, 90m, 30s); off: never",
)];

/// The keys of one harness, with their help.
pub fn keys(harness: Harness) -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    let only = match harness {
        Harness::Claude => CLAUDE_KEYS,
        Harness::Codex => &[],
    };
    KEYS.iter().chain(only)
}

/// The one line of help for a key, without its harness.
pub fn help(key: &str) -> &'static str {
    KEYS.iter()
        .chain(CLAUDE_KEYS)
        .find(|(k, _)| *k == key)
        .map_or("", |(_, h)| h)
}

pub const TEMPLATE: &str = r#"# hats configuration. Edit it here or with `hats config set <key> <value>`,
# for example `hats config set claude.threshold 85`. `hats config` lists every key.

[claude]
# auto: when the current account passes `threshold`, new sessions start on another
#       account, and sessions started with `hats run` move when the agent is idle.
# manual: only `hats use` changes the account.
mode = "auto"
# Percent of the 5-hour or weekly limit that triggers a switch (1-100).
threshold = 90
# headroom: switch to the account with the most limit left.
# next: switch to the next account in `order` that is below the threshold.
strategy = "headroom"
# Preferred order of account labels; accounts not listed follow by name.
order = []
# Running `hats run` sessions: idle = relaunch them on the new account when the
# agent is idle, resuming the same conversation; off = leave them where they are.
relaunch = "idle"
# Close a `hats run` session after this long without use: the clock starts when
# a turn ends and restarts with a prompt, a turn end or a write to the session's
# transcript. 32 or "32h" hours, "90m" minutes, "30s" seconds; 0 or "off" never.
# It prints how to resume the session.
close_idle = "32h"

[codex]
mode = "auto"
threshold = 90
strategy = "headroom"
order = []
relaunch = "idle"
"#;

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let doc: DocumentMut = text
            .parse()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut config = Self::default();
        for harness in Harness::ALL {
            let Some(table) = doc.get(harness.name()) else {
                continue;
            };
            for (key, _) in keys(harness) {
                if let Some(item) = table.get(key) {
                    let full = format!("{}.{key}", harness.name());
                    config
                        .apply(harness, key, item)
                        .map_err(|e| format!("{}: {full}: {e}", path.display()))?;
                }
            }
        }
        Ok(config)
    }

    pub fn for_harness(&self, harness: Harness) -> &HarnessConfig {
        match harness {
            Harness::Claude => &self.claude,
            Harness::Codex => &self.codex,
        }
    }

    fn for_harness_mut(&mut self, harness: Harness) -> &mut HarnessConfig {
        match harness {
            Harness::Claude => &mut self.claude,
            Harness::Codex => &mut self.codex,
        }
    }

    /// Every key with its current value, e.g. `("claude.mode", "auto")`.
    pub fn entries(&self) -> Vec<(String, String)> {
        Harness::ALL
            .iter()
            .flat_map(|&h| {
                keys(h).map(move |(key, _)| (format!("{}.{key}", h.name()), self.value(h, key)))
            })
            .collect()
    }

    pub fn get(&self, full_key: &str) -> Result<String, String> {
        let (harness, key) = split_key(full_key)?;
        Ok(self.value(harness, key))
    }

    fn value(&self, harness: Harness, key: &str) -> String {
        match key {
            "close_idle" => show_close_idle(self.close_idle),
            _ => self.for_harness(harness).get(key),
        }
    }

    fn apply(&mut self, harness: Harness, key: &str, item: &Item) -> Result<(), String> {
        if key != "close_idle" {
            return self.for_harness_mut(harness).apply(key, item);
        }
        self.close_idle = match (item.as_integer(), item.as_str()) {
            (Some(hours), _) => parse_close_idle(&hours.to_string())?,
            (_, Some(text)) => parse_close_idle(text)?,
            _ => return Err(r#"expected hours like 32, or a text like "90m""#.to_owned()),
        };
        Ok(())
    }
}

impl HarnessConfig {
    fn get(&self, key: &str) -> String {
        match key {
            "mode" => self.mode.as_str().to_owned(),
            "threshold" => self.threshold.to_string(),
            "strategy" => self.strategy.as_str().to_owned(),
            "order" => self.order.join(","),
            "relaunch" => self.relaunch.as_str().to_owned(),
            _ => unreachable!("keys come from KEYS"),
        }
    }

    fn apply(&mut self, key: &str, item: &Item) -> Result<(), String> {
        let text = |item: &Item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "expected a quoted string".to_owned())
        };
        match key {
            "mode" => self.mode = text(item)?.parse()?,
            "strategy" => self.strategy = text(item)?.parse()?,
            "relaunch" => self.relaunch = text(item)?.parse()?,
            "threshold" => {
                let n = item.as_integer().ok_or("expected a number from 1 to 100")?;
                self.threshold = parse_threshold(&n.to_string())?;
            }
            "order" => {
                let array = item
                    .as_array()
                    .ok_or(r#"expected a list like ["work", "personal"]"#)?;
                self.order = array
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or("labels must be quoted strings")
                    })
                    .collect::<Result<_, _>>()?;
            }
            _ => unreachable!("keys come from KEYS"),
        }
        Ok(())
    }
}

/// Sets one key in the file, creating it from the documented template the
/// first time. Validates before writing; comments and other keys are kept.
pub fn set(path: &Path, full_key: &str, raw: &str) -> Result<(), String> {
    let (harness, key) = split_key(full_key)?;
    let item = match key {
        "mode" => value(raw.parse::<Mode>()?.as_str()),
        "strategy" => value(raw.parse::<Strategy>()?.as_str()),
        "relaunch" => value(raw.parse::<Relaunch>()?.as_str()),
        "threshold" => value(i64::from(parse_threshold(raw)?)),
        "close_idle" => value(show_close_idle(parse_close_idle(raw)?)),
        "order" => {
            let labels = raw.split(',').map(str::trim).filter(|s| !s.is_empty());
            let mut array = Array::new();
            for label in labels {
                // `default`, the harness's own directory, can be ordered too.
                if label != crate::account::DEFAULT_LABEL {
                    crate::account::validate_label(label)?;
                }
                array.push(label);
            }
            Item::Value(Value::Array(array))
        }
        _ => unreachable!("split_key checks the key"),
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => TEMPLATE.to_owned(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if doc.get(harness.name()).is_none() {
        doc[harness.name()] = toml_edit::table();
    }
    doc[harness.name()][key] = item;
    write_new(path, doc.to_string().as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the template if there is no file yet, so `hats config edit` opens
/// a documented file.
pub fn ensure_file(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    write_new(path, TEMPLATE.as_bytes())
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        crate::account::create_private_dir(dir)?;
    }
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    fs::rename(tmp, path)
}

fn split_key(full_key: &str) -> Result<(Harness, &str), String> {
    let unknown = || {
        let keys: Vec<_> = KEYS.iter().map(|(k, _)| format!("claude.{k}")).collect();
        format!(
            "unknown key '{full_key}': use claude.<key> or codex.<key>, for example {}",
            keys.join(", ")
        )
    };
    let (section, key) = full_key.split_once('.').ok_or_else(unknown)?;
    let harness = Harness::ALL
        .into_iter()
        .find(|h| h.name() == section)
        .ok_or_else(unknown)?;
    if harness == Harness::Codex && CLAUDE_KEYS.iter().any(|(k, _)| *k == key) {
        return Err(format!(
            "{key} is Claude Code only: a Codex session has no `hats run` parent to close it"
        ));
    }
    let key = keys(harness)
        .map(|(k, _)| *k)
        .find(|k| *k == key)
        .ok_or_else(unknown)?;
    Ok((harness, key))
}

/// `32` or `32h` hours, `90m` minutes, `30s` seconds; `0` or `off` never.
fn parse_close_idle(raw: &str) -> Result<Option<Duration>, String> {
    let raw = raw.trim();
    if raw == "off" {
        return Ok(None);
    }
    let (number, unit) = match raw.as_bytes().last() {
        Some(b'h') => (&raw[..raw.len() - 1], 3600),
        Some(b'm') => (&raw[..raw.len() - 1], 60),
        Some(b's') => (&raw[..raw.len() - 1], 1),
        _ => (raw, 3600),
    };
    let secs = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .ok_or_else(|| {
            format!("close_idle must be whole hours like 32, or 90m, 30s, or off; got '{raw}'")
        })?;
    Ok((secs > 0).then(|| Duration::from_secs(secs)))
}

/// The largest unit that keeps it whole: `32h`, `90m`, `30s`, or `off`.
pub fn show_close_idle(close_idle: Option<Duration>) -> String {
    let Some(after) = close_idle else {
        return "off".to_owned();
    };
    match after.as_secs() {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn parse_threshold(raw: &str) -> Result<u8, String> {
    match raw.trim().parse::<u8>() {
        Ok(n @ 1..=100) => Ok(n),
        _ => Err(format!(
            "threshold must be a whole number from 1 to 100, got '{raw}'"
        )),
    }
}

macro_rules! choice {
    ($ty:ident { $($variant:ident => $text:literal),+ }) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self { $($ty::$variant => $text),+ }
            }
        }
        impl FromStr for $ty {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, String> {
                match s {
                    $($text => Ok($ty::$variant),)+
                    _ => Err(format!(
                        "'{s}' is not one of: {}",
                        [$($text),+].join(", ")
                    )),
                }
            }
        }
    };
}

choice!(Mode { Auto => "auto", Manual => "manual" });
choice!(Strategy { Headroom => "headroom", Next => "next" });
choice!(Relaunch { Idle => "idle", Off => "off" });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_the_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, TEMPLATE).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert_eq!(
            Config::load(&dir.path().join("missing")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn set_validates_and_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hats/config.toml");
        set(&path, "claude.threshold", "85").unwrap();
        set(&path, "codex.mode", "manual").unwrap();
        set(&path, "claude.order", "work, default, personal").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.claude.threshold, 85);
        assert_eq!(config.codex.mode, Mode::Manual);
        assert_eq!(config.claude.order, ["work", "default", "personal"]);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Percent of the 5-hour or weekly limit"));

        for (key, bad) in [
            ("claude.threshold", "0"),
            ("claude.threshold", "101"),
            ("claude.mode", "sometimes"),
            ("claude.order", "ok,../bad"),
            ("claude.nope", "1"),
            ("gemini.mode", "auto"),
        ] {
            assert!(set(&path, key, bad).is_err(), "{key}={bad}");
        }
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hats/config.toml");
        set(&path, "claude.threshold", "85").unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn close_idle_takes_hours_a_unit_or_off_and_only_for_claude() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let get = || {
            Config::load(&path)
                .unwrap()
                .get("claude.close_idle")
                .unwrap()
        };
        assert_eq!(get(), "32h", "the default, with no file");
        for (raw, shown) in [
            ("32", "32h"),
            ("5h", "5h"),
            ("90m", "90m"),
            ("120m", "2h"),
            ("30s", "30s"),
            ("0", "off"),
            ("off", "off"),
            ("1", "1h"),
        ] {
            set(&path, "claude.close_idle", raw).unwrap();
            assert_eq!(get(), shown, "{raw}");
        }
        for bad in ["-1", "1.5", "1.5h", "2d", "h", "", "never"] {
            assert!(set(&path, "claude.close_idle", bad).is_err(), "{bad}");
        }
        let err = set(&path, "codex.close_idle", "1").unwrap_err();
        assert!(err.contains("Claude Code only"), "{err}");
        assert!(
            Config::load(&path)
                .unwrap()
                .get("codex.close_idle")
                .is_err()
        );
        assert!(
            !Config::default()
                .entries()
                .iter()
                .any(|(k, _)| k == "codex.close_idle")
        );

        fs::write(&path, "[claude]\nclose_idle = 12\n").unwrap();
        assert_eq!(get(), "12h", "a bare number in the file is hours too");
        fs::write(&path, "[claude]\nclose_idle = 1.5\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.contains("claude.close_idle"), "{err}");
    }

    #[test]
    fn load_names_the_bad_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[codex]\nthreshold = 300\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(
            err.contains("codex.threshold") && err.contains("1 to 100"),
            "{err}"
        );
    }
}
