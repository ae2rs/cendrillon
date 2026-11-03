//! Running git and reading what it keeps on disk. No git library: git itself is the authority
//! on its worktrees, so cendrillon asks it, with the caller's git environment cleared and never
//! waiting on it forever.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Long enough for `git status` in a large worktree on a cold cache.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// Where git usually is, for a service manager that sets no `PATH`.
const DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

/// The git binary: from `PATH`, else the usual places. `None` when there is none.
pub fn binary() -> Option<&'static Path> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(find).as_deref()
}

fn find() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .chain(DIRS.iter().map(PathBuf::from))
        .map(|dir| dir.join("git"))
        .find(|candidate| usable(candidate))
}

fn usable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let executable = fs::metadata(candidate)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
    // Without the command line tools, macOS's `/usr/bin/git` asks the user to install them.
    #[cfg(target_os = "macos")]
    if executable && candidate == Path::new("/usr/bin/git") {
        return Command::new("/usr/bin/xcode-select")
            .arg("-p")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
    }
    executable
}

/// A git command run in `dir`. Variables such as `GIT_DIR` that would point it elsewhere are
/// cleared; it takes no optional lock, so `git status` leaves the index, whose modification
/// time tells when someone last used the worktree, alone; and it starts no file system monitor,
/// a daemon that would stay in the worktree.
pub fn command(dir: &Path) -> Result<Command> {
    let git = binary().context("git not found")?;
    let mut command = Command::new(git);
    for (name, _) in std::env::vars_os() {
        if name.as_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(dir);
    Ok(command)
}

/// What a git command printed.
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    /// Its standard output, or an error with what git said.
    pub fn checked(self) -> Result<Vec<u8>> {
        if !self.status.success() {
            bail!("{}", self.message());
        }
        Ok(self.stdout)
    }

    /// The gist of what git said on failure.
    pub fn message(&self) -> String {
        let said = self
            .stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no message");
        format!("git exited with {}: {said}", self.status)
    }
}

/// Runs `command`, killing it after `timeout`.
pub fn run(mut command: Command, timeout: Duration) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("running git")?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git took over {}", humantime::format_duration(timeout));
        }
        thread::sleep(Duration::from_millis(5));
    };
    let join = |handle: thread::JoinHandle<io::Result<Vec<u8>>>| {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("reading git's output panicked"))?
            .context("reading git's output")
    };
    Ok(Output {
        status,
        stdout: join(stdout)?,
        stderr: String::from_utf8_lossy(&join(stderr)?).into_owned(),
    })
}

/// Reads a pipe to its end on another thread, so a child filling the other pipe never blocks.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut bytes)?;
        }
        Ok(bytes)
    })
}

/// Runs a git command in `dir` and returns its standard output.
pub fn output<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Vec<u8>> {
    let mut command = command(dir)?;
    command.args(args);
    run(command, TIMEOUT)?.checked()
}

/// Like [`output`], as trimmed text.
pub fn text<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<String> {
    Ok(String::from_utf8_lossy(&output(dir, args)?)
        .trim()
        .to_owned())
}

/// Makes a command finish even if cendrillon is stopped meanwhile: in its own process group,
/// so a Ctrl-C in the terminal does not reach it, and deaf to SIGTERM, which systemd sends to
/// every process of the service when it stops it. A worktree removal cut short leaves half a
/// worktree that looks modified, and is never removed.
pub fn shield(command: &mut Command) {
    command.process_group(0);
    // SAFETY: `signal` is async-signal-safe, and the ignored dispositions survive `exec`.
    unsafe {
        command.pre_exec(|| {
            use nix::sys::signal::{SigHandler, Signal, signal};
            for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
                signal(sig, SigHandler::SigIgn).map_err(io::Error::from)?;
            }
            Ok(())
        });
    }
}

/// One entry of `git worktree list --porcelain -z`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// The full name of the branch checked out, `None` when HEAD is detached.
    pub branch: Option<String>,
    pub bare: bool,
    /// Why it is locked, possibly empty, when it is.
    pub locked: Option<String>,
    /// Its directory is gone.
    pub prunable: bool,
}

/// Parses `git worktree list --porcelain -z`: attributes ending in NUL, entries ending in an
/// empty one. The first entry is the main worktree, or the repository itself when bare.
pub fn parse_worktrees(bytes: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    for field in bytes.split(|&byte| byte == 0) {
        if field.is_empty() {
            entries.extend(current.take());
            continue;
        }
        let (key, value) = match field.iter().position(|&byte| byte == b' ') {
            Some(at) => (&field[..at], Some(&field[at + 1..])),
            None => (field, None),
        };
        let text = || String::from_utf8_lossy(value.unwrap_or_default()).into_owned();
        if key == b"worktree" {
            entries.extend(current.take());
            current = Some(Entry {
                path: PathBuf::from(OsStr::from_bytes(value.unwrap_or_default())),
                ..Entry::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        match key {
            b"HEAD" => entry.head = Some(text()),
            b"branch" => entry.branch = Some(text()),
            b"bare" => entry.bare = true,
            b"locked" => entry.locked = Some(text()),
            b"prunable" => entry.prunable = true,
            _ => {}
        }
    }
    entries.extend(current);
    entries
}

/// What `git status --porcelain=v1 -z --ignored=matching` reports.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Changes to tracked files, staged or not.
    pub changes: bool,
    pub untracked: bool,
    /// Ignored paths relative to the worktree; directories end in `/` and are not entered.
    pub ignored: Vec<String>,
}

pub fn parse_status(bytes: &[u8]) -> Status {
    let mut status = Status::default();
    let mut fields = bytes.split(|&byte| byte == 0).filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            status.changes = true;
            continue;
        }
        let (code, path) = (&field[..2], &field[3..]);
        match code {
            b"!!" => status
                .ignored
                .push(String::from_utf8_lossy(path).into_owned()),
            b"??" => status.untracked = true,
            _ => {
                status.changes = true;
                // A rename or copy names its source next.
                if code.contains(&b'R') || code.contains(&b'C') {
                    fields.next();
                }
            }
        }
    }
    status
}

/// The administrative directory of the linked worktree at `worktree`, from its `.git` file:
/// `<common git dir>/worktrees/<name>`. `None` for anything else, a main worktree or a
/// submodule included.
pub fn admin_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    if !fs::symlink_metadata(&dot_git).ok()?.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let gitdir = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    // Relative with `worktree.useRelativePaths`.
    let admin = fs::canonicalize(worktree.join(gitdir)).ok()?;
    is_file(&admin.join("commondir")).then_some(admin)
}

/// The common git directory an administrative directory belongs to.
pub fn common_dir(admin: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(admin.join("commondir")).ok()?;
    fs::canonicalize(admin.join(text.trim())).ok()
}

/// The repository whose `.git` entry is `dot_git`, as its common git directory, when it has
/// linked worktrees: a linked worktree's `.git` file names it, and a main worktree's `.git`
/// directory has them in `worktrees/`.
pub fn repo_of(dot_git: &Path) -> Option<PathBuf> {
    let meta = fs::symlink_metadata(dot_git).ok()?;
    if meta.is_dir() {
        let has_worktrees = fs::read_dir(dot_git.join("worktrees"))
            .is_ok_and(|mut entries| entries.next().is_some());
        return has_worktrees.then(|| fs::canonicalize(dot_git).ok())?;
    }
    common_dir(&admin_dir(dot_git.parent()?)?)
}

/// The repository with linked worktrees that holds `path`, if any: the nearest `.git` above.
pub fn repo_containing(path: &Path) -> Option<PathBuf> {
    let dot_git = path
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|dot_git| fs::symlink_metadata(dot_git).is_ok())?;
    repo_of(&dot_git)
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Matches `text` against a glob: `*` and `?` stop at `/`, `**` does not. Anything else is
/// literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn matches(pattern: &[u8], text: &[u8]) -> bool {
        match pattern {
            [] => text.is_empty(),
            [b'*', b'*', rest @ ..] => {
                // `**/` also matches no directory at all: `a/**/b` matches `a/b`.
                (0..=text.len()).any(|at| matches(rest, &text[at..]))
                    || rest
                        .strip_prefix(b"/")
                        .is_some_and(|rest| matches(rest, text))
            }
            [b'*', rest @ ..] => {
                let end = text.iter().position(|&b| b == b'/').unwrap_or(text.len());
                (0..=end).any(|at| matches(rest, &text[at..]))
            }
            [b'?', rest @ ..] => {
                text.first().is_some_and(|&b| b != b'/') && matches(rest, &text[1..])
            }
            [first, rest @ ..] => text.first() == Some(first) && matches(rest, &text[1..]),
        }
    }
    matches(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
pub mod fixtures {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Runs git in `dir` with a fixed identity, failing the test if it fails.
    pub fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?} failed: {stderr}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A repository at `<dir>/repo` with one commit on `main` ignoring `target/`, `*.log`
    /// and `.env`.
    pub fn repo(dir: &Path) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), "target/\n*.log\n.env\n").unwrap();
        std::fs::write(repo.join("README"), "hello\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        std::fs::canonicalize(repo).unwrap()
    }

    /// Makes a worktree look untouched since `time`: its git files and its top directory.
    pub fn backdate(worktree: &Path, time: std::time::SystemTime) {
        let admin = super::admin_dir(worktree).unwrap();
        for path in [
            admin.join("HEAD"),
            admin.join("index"),
            admin.join("logs/HEAD"),
            worktree.to_path_buf(),
        ] {
            crate::inventory::fixtures::set_mtime(&path, time);
        }
    }

    /// Adds a worktree of `repo` at `path` on a new branch `branch`.
    pub fn worktree(repo: &Path, path: &Path, branch: &str) -> PathBuf {
        git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
            ],
        );
        std::fs::canonicalize(path).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn parses_worktree_list() {
        let list = b"worktree /r\0HEAD 1111\0branch refs/heads/main\0\0\
                     worktree /w/a b\0HEAD 2222\0detached\0locked agent at work\0\0\
                     worktree /w/gone\0HEAD 3333\0branch refs/heads/x\0locked\0\
                     prunable gitdir file points to non-existent location\0\0";
        let entries = parse_worktrees(list);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(entries[1].path, Path::new("/w/a b"));
        assert_eq!(entries[1].branch, None);
        assert_eq!(entries[1].locked.as_deref(), Some("agent at work"));
        assert_eq!(entries[2].locked.as_deref(), Some(""));
        assert!(entries[2].prunable && !entries[1].prunable);
        assert!(parse_worktrees(b"worktree /bare.git\0bare\0\0")[0].bare);
    }

    #[test]
    fn parses_status() {
        let status = parse_status(b"!! .env\0!! target/\0");
        assert_eq!(
            status,
            Status {
                changes: false,
                untracked: false,
                ignored: vec![".env".into(), "target/".into()],
            }
        );
        assert!(parse_status(b"?? new\0").untracked);
        let renamed = parse_status(b"R  new\0old\0!! a.log\0");
        assert!(renamed.changes && !renamed.untracked);
        assert_eq!(renamed.ignored, ["a.log"]);
        assert_eq!(parse_status(b""), Status::default());
    }

    #[test]
    fn globs() {
        assert!(glob_match(".env", ".env"));
        assert!(glob_match(".env.*", ".env.local"));
        assert!(!glob_match(".env.*", ".env"));
        assert!(glob_match("*.local.*", "settings.local.json"));
        assert!(!glob_match("*.local.*", "a/settings.local.json"));
        assert!(glob_match("/home/me/keep-*", "/home/me/keep-this"));
        assert!(!glob_match("/home/me/keep-*", "/home/me/keep-this/sub"));
        assert!(glob_match("/tmp/**/wt", "/tmp/claude-1/scratchpad/wt"));
        assert!(glob_match("/tmp/**/wt", "/tmp/wt"));
        assert!(glob_match("**", "a/b"));
        assert!(glob_match("a?c", "abc") && !glob_match("a?c", "a/c"));
    }

    #[test]
    fn finds_the_repository_from_either_kind_of_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        let common = repo.join(".git");
        // No linked worktree yet: nothing to look after.
        assert_eq!(repo_of(&common), None);
        let wt = worktree(&repo, &dir.path().join("wt"), "feature");
        assert_eq!(repo_of(&common), Some(common.clone()));
        assert_eq!(repo_of(&wt.join(".git")), Some(common.clone()));
        assert_eq!(admin_dir(&wt), Some(common.join("worktrees/wt")));
        assert_eq!(admin_dir(&repo), None);
        std::fs::create_dir_all(wt.join("src/deep")).unwrap();
        assert_eq!(repo_containing(&wt.join("src/deep")), Some(common.clone()));
        assert_eq!(repo_containing(dir.path()), None);
    }

    #[test]
    fn runs_git_outside_the_callers_repository_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        let top = text(&repo, &["rev-parse", "--show-toplevel"]).unwrap();
        assert_eq!(Path::new(&top), repo);
        assert!(text(dir.path(), &["rev-parse", "--show-toplevel"]).is_err());

        let mut slow = Command::new("sleep");
        slow.arg("5");
        let started = Instant::now();
        assert!(run(slow, Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
