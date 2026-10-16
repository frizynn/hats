//! `config.toml`: how each harness switches accounts. Read with
//! validation that names the key and the allowed values; edited in place by
//! `hats config set`, keeping the user's comments.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::str::FromStr;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub claude: HarnessConfig,
    pub codex: HarnessConfig,
}

/// What `hats config` shows and accepts, with one line of help each.
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
            let section = config.for_harness_mut(harness);
            for (key, _) in KEYS {
                if let Some(item) = table.get(key) {
                    let full = format!("{}.{key}", harness.name());
                    section
                        .apply(key, item)
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
            .flat_map(|h| {
                let c = self.for_harness(*h);
                KEYS.iter()
                    .map(move |(key, _)| (format!("{}.{key}", h.name()), c.get(key)))
            })
            .collect()
    }

    pub fn get(&self, full_key: &str) -> Result<String, String> {
        let (harness, key) = split_key(full_key)?;
        Ok(self.for_harness(harness).get(key))
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
    let key = KEYS
        .iter()
        .map(|(k, _)| *k)
        .find(|k| *k == key)
        .ok_or_else(unknown)?;
    Ok((harness, key))
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
