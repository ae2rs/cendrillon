use std::path::PathBuf;

use anyhow::{Context, Result};
use nix::unistd::{User, getuid};

/// The user whose caches are cleaned, and where cendrillon keeps its own files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    pub home: PathBuf,
    pub user: String,
}

impl Env {
    /// The current user, from the password database rather than `$HOME`, which the service
    /// manager does not always set.
    pub fn current() -> Result<Self> {
        let uid = getuid();
        let user = User::from_uid(uid)?.with_context(|| format!("no user with uid {uid}"))?;
        Ok(Self {
            home: user.dir,
            user: user.name,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join(".config/cendrillon/config.toml")
    }

    #[cfg(target_os = "macos")]
    pub fn state_file(&self) -> PathBuf {
        self.home
            .join("Library/Application Support/cendrillon/state.json")
    }

    /// `~/.local/state`, the XDG default, rather than `$XDG_STATE_HOME`: the command line and
    /// the systemd service must agree, and the service may not see the shell's variables.
    #[cfg(target_os = "linux")]
    pub fn state_file(&self) -> PathBuf {
        self.home.join(".local/state/cendrillon/state.json")
    }

    #[cfg(target_os = "macos")]
    pub fn log_file(&self) -> PathBuf {
        self.home.join("Library/Logs/cendrillon.log")
    }

    #[cfg(target_os = "macos")]
    pub fn launch_agent_file(&self, label: &str) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"))
    }

    #[cfg(target_os = "linux")]
    pub fn systemd_unit_file(&self, unit: &str) -> PathBuf {
        self.home.join(".config/systemd/user").join(unit)
    }
}
