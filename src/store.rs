//! Where this tool keeps things. Account directories are data
//! (`$XDG_DATA_HOME`), the registry and the current-account pointers are state
//! (`$XDG_STATE_HOME`), following the XDG Base Directory Specification on
//! every platform so the paths are predictable for other tools.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::account::{Account, DEFAULT_LABEL, Harness, create_private_dir};

pub const DATA_ENV: &str = "HATS_HOME";
pub const STATE_ENV: &str = "HATS_STATE_DIR";
const APP: &str = "hats";

pub struct Store {
    pub home: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
}

impl Store {
    pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, String> {
        let non_empty = |key: &str| var(key).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = non_empty("HOME").ok_or("HOME is not set")?;
        let data = non_empty(DATA_ENV).unwrap_or_else(|| {
            non_empty("XDG_DATA_HOME")
                .unwrap_or_else(|| home.join(".local/share"))
                .join(APP)
        });
        let state = non_empty(STATE_ENV).unwrap_or_else(|| {
            non_empty("XDG_STATE_HOME")
                .unwrap_or_else(|| home.join(".local/state"))
                .join(APP)
        });
        Ok(Self { home, data, state })
    }

    pub fn accounts_dir(&self, harness: Harness) -> PathBuf {
        self.data.join(harness.name())
    }

    pub fn registry(&self) -> PathBuf {
        self.state.join(crate::registry::FILE_NAME)
    }

    pub fn ensure_state(&self) -> io::Result<()> {
        create_private_dir(&self.state)
    }

    fn current_file(&self, harness: Harness) -> PathBuf {
        self.state.join(format!("current-{}", harness.name()))
    }

    /// The account new launches use when no label is given.
    pub fn current(&self, harness: Harness) -> Result<Account, String> {
        let label = match fs::read_to_string(self.current_file(harness)) {
            Ok(label) => label.trim().to_owned(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => DEFAULT_LABEL.to_owned(),
            Err(e) => return Err(e.to_string()),
        };
        Account::find(harness, &label, self)
    }

    pub fn set_current(&self, account: &Account) -> io::Result<()> {
        self.ensure_state()?;
        let path = self.current_file(account.harness);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, format!("{}\n", account.label))?;
        fs::rename(tmp, path)
    }
}
