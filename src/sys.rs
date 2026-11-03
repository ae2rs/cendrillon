//! Thin wrappers over the system calls the cleaner needs.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, fcntl};
use nix::libc;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use walkdir::WalkDir;

/// An exclusive advisory lock, released when dropped.
pub struct FileLock {
    _file: File,
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Takes, without blocking, the lock Bazel takes on its lock files: an `fcntl` record lock on
/// byte 0 (`blaze_util_posix.cc`, and Java's whole-file `FileChannel` locks, overlap it).
/// Returns `None` when another process holds it.
///
/// POSIX record locks belong to the process and are dropped when *any* descriptor of the
/// file is closed, so nothing else in this process may open a locked file.
pub fn try_lock(path: &Path) -> io::Result<Option<FileLock>> {
    let file = open_lock_file(path)?;
    let request = libc::flock {
        l_start: 0,
        l_len: 1,
        l_pid: 0,
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
    };
    match fcntl(&file, FcntlArg::F_SETLK(&request)) {
        Ok(_) => Ok(Some(FileLock { _file: file })),
        Err(Errno::EAGAIN | Errno::EACCES) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Takes, without blocking, an exclusive `flock` lock: the kind Cargo takes on its
/// `.cargo-lock` files through `File::try_lock`. Returns `None` when another process holds
/// it, shared or exclusive.
pub fn try_flock(path: &Path) -> io::Result<Option<FileLock>> {
    let file = open_lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(FileLock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub available: u64,
    pub total: u64,
}

impl DiskSpace {
    pub fn of(path: &Path) -> io::Result<Self> {
        let stat = nix::sys::statvfs::statvfs(path)?;
        let block = stat.fragment_size() as u64;
        Ok(Self {
            available: stat.blocks_available() as u64 * block,
            total: stat.blocks() as u64 * block,
        })
    }

    pub fn free_percent(self) -> f64 {
        if self.total == 0 {
            return 100.0;
        }
        self.available as f64 * 100.0 / self.total as f64
    }

    /// Bytes to free before `percent` of the disk is available.
    pub fn shortfall(self, percent: u8) -> u64 {
        (self.total / 100 * u64::from(percent)).saturating_sub(self.available)
    }
}

/// The pid of the Bazel server that owns `output_base`, if one is running.
///
/// The pid file can outlive its server, so the process must also name this output base on its
/// command line; otherwise a reused pid could belong to anything.
pub fn bazel_server(output_base: &Path) -> Option<Pid> {
    let text = std::fs::read_to_string(output_base.join("server/server.pid.txt")).ok()?;
    let pid = Pid::from_raw(text.trim().parse().ok()?);
    kill(pid, None).ok()?;
    let named = output_base_flag(pid)?;
    let same = named == output_base
        || std::fs::canonicalize(&named).ok()? == std::fs::canonicalize(output_base).ok()?;
    same.then_some(pid)
}

/// The `--output_base` a process was started with.
#[cfg(target_os = "macos")]
fn output_base_flag(pid: Pid) -> Option<PathBuf> {
    let ps = Command::new("/bin/ps")
        .args(["-ww", "-o", "command=", "-p"])
        .arg(pid.to_string())
        .output()
        .ok()?;
    let command = String::from_utf8_lossy(&ps.stdout);
    // The value runs to the next flag, so paths with spaces survive.
    let (_, rest) = command.split_once("--output_base=")?;
    Some(PathBuf::from(rest.split(" --").next()?.trim_end()))
}

/// The `--output_base` a process was started with. `/proc` separates the arguments, so the
/// value is exact.
#[cfg(target_os = "linux")]
fn output_base_flag(pid: Pid) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    cmdline
        .split(|&byte| byte == 0)
        .find_map(|arg| arg.strip_prefix(b"--output_base="))
        .map(|value| PathBuf::from(std::ffi::OsStr::from_bytes(value)))
}

/// The command line of every running process, its arguments joined by spaces.
#[cfg(target_os = "macos")]
pub fn command_lines() -> Vec<String> {
    let Ok(ps) = Command::new("/bin/ps")
        .args(["-axww", "-o", "command="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The command line of every running process, its arguments joined by spaces.
#[cfg(target_os = "linux")]
pub fn command_lines() -> Vec<String> {
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    procs
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.parse::<u32>().is_ok())
        })
        .filter_map(|entry| std::fs::read(entry.path().join("cmdline")).ok())
        .filter(|cmdline| !cmdline.is_empty())
        .map(|cmdline| {
            let joined: Vec<u8> = cmdline
                .strip_suffix(&[0])
                .unwrap_or(&cmdline)
                .iter()
                .map(|&byte| if byte == 0 { b' ' } else { byte })
                .collect();
            String::from_utf8_lossy(&joined).into_owned()
        })
        .collect()
}

/// Paths other processes are using: their working directory, root directory, executable and
/// open files, with mapped files on Linux. Only processes this user may inspect are seen,
/// which is every process of their own.
pub fn open_paths() -> Vec<(Pid, PathBuf)> {
    let own = std::process::id() as i32;
    let mut found = Vec::new();
    for pid in all_pids().into_iter().filter(|&pid| pid != own) {
        found.extend(
            paths_of(pid)
                .into_iter()
                .filter(|path| path.is_absolute())
                .map(|path| (Pid::from_raw(pid), path)),
        );
    }
    found
}

#[cfg(target_os = "linux")]
fn all_pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}

/// Read from `/proc`. A file deleted while open reads `<path> (deleted)`, which still names
/// the directory it was in.
#[cfg(target_os = "linux")]
fn paths_of(pid: i32) -> Vec<PathBuf> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let mut paths: Vec<PathBuf> = ["cwd", "root", "exe"]
        .iter()
        .filter_map(|link| std::fs::read_link(proc.join(link)).ok())
        .collect();
    paths.extend(
        std::fs::read_dir(proc.join("fd"))
            .into_iter()
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok()),
    );
    // Shared libraries stay mapped after their file is closed.
    let maps = std::fs::read_to_string(proc.join("maps")).unwrap_or_default();
    let mut mapped: Vec<PathBuf> = maps
        .lines()
        .filter_map(|line| line.splitn(6, ' ').nth(5))
        .map(str::trim_start)
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    mapped.dedup();
    paths.extend(mapped);
    paths
}

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<i32> {
    // SAFETY: called with a null buffer, it returns how many pids there are; then fills at
    // most the buffer's size and returns how many it wrote.
    unsafe {
        let count = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if count <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0 as libc::pid_t; count as usize + 64];
        let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let count = libc::proc_listallpids(pids.as_mut_ptr().cast(), size);
        pids.truncate(count.max(0) as usize);
        pids
    }
}

/// `struct proc_fileinfo` of `<sys/proc_info.h>`, which `libc` lacks.
#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: libc::off_t,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath`, what `PROC_PIDFDVNODEPATHINFO` returns.
#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

// `PROC_PIDFDVNODEPATHINFO_SIZE`: the call fails on any other size.
#[cfg(target_os = "macos")]
const _: () = assert!(std::mem::size_of::<VnodeFdInfoWithPath>() == 1200);

/// Read through libproc, as `lsof` does.
#[cfg(target_os = "macos")]
fn paths_of(pid: i32) -> Vec<PathBuf> {
    use std::mem::{size_of, zeroed};
    use std::os::unix::ffi::OsStrExt;
    fn path(raw: &[[libc::c_char; 32]; 32]) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        let bytes: Vec<u8> = raw
            .iter()
            .flatten()
            .map(|&c| c as u8)
            .take_while(|&b| b != 0)
            .collect();
        (!bytes.is_empty()).then(|| PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    let mut paths = Vec::new();
    // SAFETY: each call is given a buffer of the size it is told, of the type the flavor
    // fills, and its result is only read when the call filled it whole.
    unsafe {
        let mut dirs: libc::proc_vnodepathinfo = zeroed();
        let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let filled = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut dirs).cast(),
            size,
        );
        if filled == size {
            paths.extend(path(&dirs.pvi_cdir.vip_path));
            paths.extend(path(&dirs.pvi_rdir.vip_path));
        }

        let mut exe = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len = libc::proc_pidpath(pid, exe.as_mut_ptr().cast(), exe.len() as u32);
        if len > 0 {
            exe.truncate(len as usize);
            paths.push(PathBuf::from(std::ffi::OsStr::from_bytes(&exe)));
        }

        let fd_size = size_of::<libc::proc_fdinfo>();
        let bytes = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
        if bytes <= 0 {
            return paths;
        }
        let mut fds: Vec<libc::proc_fdinfo> = vec![zeroed(); bytes as usize / fd_size + 32];
        let bytes = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (fds.len() * fd_size) as libc::c_int,
        );
        fds.truncate(bytes.max(0) as usize / fd_size);
        for fd in fds {
            if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
                continue;
            }
            let mut info: VnodeFdInfoWithPath = zeroed();
            let size = size_of::<VnodeFdInfoWithPath>() as libc::c_int;
            let filled = libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                (&raw mut info).cast(),
                size,
            );
            if filled == size {
                paths.extend(path(&info.pvip.vip_path));
            }
        }
    }
    paths
}

/// Sets a file's access time, leaving its modification time alone.
#[cfg(target_os = "macos")]
pub fn set_accessed(path: &Path, time: SystemTime) -> io::Result<()> {
    File::open(path)?.set_times(std::fs::FileTimes::new().set_accessed(time))
}

/// Sets a file's access time, leaving its modification time alone. By path, so the file is
/// not opened: the daemon takes an open of a hash file for a read by Cargo.
#[cfg(target_os = "linux")]
pub fn set_accessed(path: &Path, time: SystemTime) -> io::Result<()> {
    use nix::sys::stat::{UtimensatFlags, utimensat};
    use nix::sys::time::TimeSpec;
    let since = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    utimensat(
        nix::fcntl::AT_FDCWD,
        path,
        &TimeSpec::from_duration(since),
        &TimeSpec::UTIME_OMIT,
        UtimensatFlags::NoFollowSymlink,
    )
    .map_err(io::Error::from)
}

/// Opens a file to read it without moving its access time, which is how cendrillon tells
/// that Cargo read a file. Linux only allows that to the file's owner, so falls back to a
/// plain open: the caller then puts the access time back.
pub fn open_quietly(path: &Path) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOATIME)
            .open(path)
        {
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => {}
            opened => return opened,
        }
    }
    File::open(path)
}

/// Stops a process: SIGTERM, then SIGKILL if it is still alive after `grace`.
pub fn terminate(pid: Pid, grace: Duration) -> io::Result<()> {
    for signal in [Signal::SIGTERM, Signal::SIGKILL] {
        match kill(pid, signal) {
            Ok(()) => {}
            Err(Errno::ESRCH) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if kill(pid, None) == Err(Errno::ESRCH) {
                return Ok(());
            }
            sleep(Duration::from_millis(100));
        }
    }
    Err(io::Error::other(format!("pid {pid} survived SIGKILL")))
}

/// Bytes allocated on disk by everything under `path`, counting each hard-linked file once.
pub fn tree_size(path: &Path) -> u64 {
    let mut seen = HashSet::new();
    WalkDir::new(path)
        .follow_root_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| allocated(&meta, &mut seen))
        .sum()
}

/// Bytes that deleting `path` is sure to free: a file also linked from elsewhere is not
/// counted.
pub fn unshared_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .follow_root_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_dir() || meta.nlink() == 1)
        .map(|meta| meta.blocks() * 512)
        .sum()
}

/// Deletes `path` and everything under it, first making read-only directories writable, as
/// Bazel leaves many of them. Symlinks are removed, never followed. Returns the bytes
/// allocated by the tree.
pub fn remove_tree(path: &Path) -> io::Result<u64> {
    let mut seen = HashSet::new();
    let mut size = 0;
    // Pre-order, so a directory is made readable before the walk lists it.
    let entries = WalkDir::new(path).follow_root_links(false);
    for entry in entries.into_iter().filter_map(Result::ok) {
        let Ok(meta) = entry.metadata() else { continue };
        size += allocated(&meta, &mut seen);
        let mode = meta.permissions().mode();
        if meta.is_dir() && mode & 0o700 != 0o700 {
            std::fs::set_permissions(entry.path(), PermissionsExt::from_mode(mode | 0o700))?;
        }
    }
    std::fs::remove_dir_all(path)?;
    Ok(size)
}

/// Allocated bytes of one file. `seen` deduplicates hard links.
pub fn allocated(meta: &std::fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
    if meta.nlink() > 1 && !meta.is_dir() && !seen.insert((meta.dev(), meta.ino())) {
        return 0;
    }
    meta.blocks() * 512
}

#[cfg(test)]
pub mod testing {
    use std::io::{BufRead, BufReader};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    /// Holds a lock on a file from another process, as a Bazel client or Cargo would, until
    /// dropped.
    pub struct LockHolder(Child);

    impl LockHolder {
        /// Bazel's `fcntl` record lock on byte 0.
        pub fn fcntl(path: &Path) -> Self {
            // struct flock on macOS: off_t l_start, off_t l_len, pid_t l_pid, short l_type,
            // short l_whence. On Linux: short l_type, short l_whence, then the offsets aligned
            // to 8 bytes, then pid_t l_pid, padded to 32 bytes.
            let layout = if cfg!(target_os = "macos") {
                r#"pack("q q l s s", 0, 1, 0, F_WRLCK, 0)"#
            } else {
                r#"pack("s s x4 q q l x4", F_WRLCK, 0, 0, 1, 0)"#
            };
            Self::spawn(
                path,
                &format!(
                    r#"my $request = {layout};
                       fcntl($f, F_SETLK, $request) or die "lock: $!";"#
                ),
            )
        }

        /// Cargo's shared `flock`, as on `.cargo-lock` during a build.
        pub fn flock_shared(path: &Path) -> Self {
            Self::spawn(path, r#"flock($f, LOCK_SH | LOCK_NB) or die "lock: $!";"#)
        }

        fn spawn(path: &Path, lock: &str) -> Self {
            let script = format!(
                r#"use Fcntl qw(:DEFAULT :flock);
                open(my $f, "+<", $ARGV[0]) or die "open: $!";
                {lock}
                $| = 1; print "locked\n"; <STDIN>;"#
            );
            let mut child = Command::new("perl")
                .args(["-e", &script])
                .arg(path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            BufReader::new(child.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "locked\n");
            Self(child)
        }
    }

    impl Drop for LockHolder {
        fn drop(&mut self) {
            drop(self.0.stdin.take());
            let _ = self.0.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::LockHolder;
    use super::*;

    #[test]
    fn lock_is_refused_while_another_process_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        std::fs::write(&path, "").unwrap();

        let holder = LockHolder::fcntl(&path);
        assert!(try_lock(&path).unwrap().is_none());

        drop(holder);
        assert!(try_lock(&path).unwrap().is_some());
    }

    #[test]
    fn flock_is_refused_while_cargo_holds_it_shared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".cargo-lock");
        std::fs::write(&path, "").unwrap();

        let holder = LockHolder::flock_shared(&path);
        assert!(try_flock(&path).unwrap().is_none());

        drop(holder);
        assert!(try_flock(&path).unwrap().is_some());
    }

    #[test]
    fn remove_tree_handles_read_only_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("base");
        let nested = root.join("external/repo");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("BUILD"), vec![0u8; 10_000]).unwrap();
        std::fs::set_permissions(&nested, PermissionsExt::from_mode(0o555)).unwrap();

        let size = remove_tree(&root).unwrap();
        assert!(size >= 10_000);
        assert!(!root.exists());
    }

    #[test]
    fn remove_tree_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("BUILD"), "").unwrap();
        let root = dir.path().join("base/execroot");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&workspace, root.join("src")).unwrap();

        remove_tree(&dir.path().join("base")).unwrap();
        assert!(workspace.join("BUILD").exists());
    }

    #[test]
    fn tree_size_counts_hard_links_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100_000]).unwrap();
        let single = tree_size(dir.path());
        std::fs::hard_link(dir.path().join("a"), dir.path().join("b")).unwrap();
        assert_eq!(tree_size(dir.path()), single);
    }

    #[test]
    fn shortfall_is_zero_when_enough_is_free() {
        let disk = DiskSpace {
            available: 30,
            total: 100,
        };
        assert_eq!(disk.shortfall(25), 0);
        assert_eq!(disk.shortfall(40), 10);
    }

    #[test]
    fn sees_a_process_working_in_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(child.id() as i32);
        let seen = open_paths()
            .iter()
            .any(|(owner, path)| *owner == pid && path.starts_with(&dir));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(seen);
    }

    #[test]
    fn no_server_without_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(bazel_server(dir.path()), None);
    }
}
