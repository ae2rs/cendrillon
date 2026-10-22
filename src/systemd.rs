//! Running the daemon at login as a systemd user service.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::env::Env;

pub const UNIT: &str = "cendrillon.service";

/// Writes the service unit pointing at the running executable, then enables and (re)starts it.
pub fn install(env: &Env) -> Result<()> {
    let exe = std::env::current_exe()?
        .canonicalize()
        .context("locating the cendrillon executable")?;
    let unit_path = env.systemd_unit_file(UNIT);
    fs::create_dir_all(unit_path.parent().context("unit path has no parent")?)?;
    fs::write(&unit_path, unit(&exe))
        .with_context(|| format!("writing {}", unit_path.display()))?;

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT])?;
    // Replaces a running instance, which may be an older binary. systemd waits for it to exit.
    systemctl(&["restart", UNIT])?;
    println!(
        "Installed {} running {}",
        unit_path.display(),
        exe.display()
    );
    println!("Logs: journalctl --user -u {UNIT}");
    Ok(())
}

/// Stops and disables the service, and removes its unit.
pub fn uninstall(env: &Env) -> Result<()> {
    let unit_path = env.systemd_unit_file(UNIT);
    let _ = systemctl(&["disable", "--now", UNIT]);
    match fs::remove_file(&unit_path) {
        Ok(()) => println!("Removed {}", unit_path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("Not installed."),
        Err(e) => return Err(e).with_context(|| format!("removing {}", unit_path.display())),
    }
    let _ = systemctl(&["daemon-reload"]);
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("running systemctl")?;
    if !output.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// A low-priority background service that starts at login and is restarted if it dies.
///
/// Nice and best-effort I/O at the lowest priority, as on macOS, rather than the `idle`
/// classes: cendrillon briefly holds Cargo's and Bazel's locks, and a starved holder would
/// stall the build waiting on them. Logs go to the journal.
fn unit(exe: &Path) -> String {
    let exe = quote(&exe.to_string_lossy());
    format!(
        "[Unit]
Description=Deletes unused Bazel caches and Cargo build output
Documentation=https://github.com/ae2rs/cendrillon

[Service]
ExecStart={exe} daemon
Restart=always
RestartSec=60
Nice=10
IOSchedulingClass=best-effort
IOSchedulingPriority=7

[Install]
WantedBy=default.target
"
    )
}

/// One `ExecStart=` word: quoted, with `\\`, `\"` and `%%` escaped, so spaces and specifiers in
/// the path are taken literally.
fn quote(word: &str) -> String {
    let escaped = word
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_quotes_the_executable() {
        let text = unit(Path::new("/home/me/my bin/100%/cendrillon"));
        assert!(text.contains("ExecStart=\"/home/me/my bin/100%%/cendrillon\" daemon\n"));
    }

    #[test]
    fn unit_is_valid() {
        // systemd-analyze checks the executable exists, so point it at a real one. The system
        // manager's checks need no user session, which CI lacks, and parse the same syntax.
        let Ok(analyze) = which("systemd-analyze") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(UNIT);
        fs::write(&path, unit(&std::env::current_exe().unwrap())).unwrap();
        let verify = Command::new(analyze)
            .arg("verify")
            .arg(&path)
            .output()
            .unwrap();
        assert!(verify.status.success(), "{verify:?}");
    }

    fn which(name: &str) -> Result<std::path::PathBuf> {
        let path = std::env::var_os("PATH").context("no PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
            .context("not found")
    }
}
