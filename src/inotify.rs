//! Watching Cargo read build units as it happens, for file systems mounted `noatime`, which
//! record no read in access times.
//!
//! Cargo opens the fingerprint hash file of every unit a build uses, even a Fresh one, and
//! inotify reports every open of a file in a watched directory: one watch per unit directory,
//! plus one per `.fingerprint/` to learn of new units. Unlike access times, this works on any
//! mount, but only while the daemon runs, and inotify cannot tell who opened the file.
//! cendrillon's own opens count too: it remembers the contents of each hash file (see
//! `cargo.rs`) and opens one again only after Cargo rewrote it, so its own open looks like a
//! read soon after a compile, which the compile already counts for.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};

use crate::cargo::{self, Observed};

/// Watches Cargo targets for reads of their hash files, from a background thread.
pub struct Reads {
    shared: Arc<Shared>,
}

struct Shared {
    inotify: Inotify,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    watches: HashMap<WatchDescriptor, Watched>,
    /// By target root.
    reads: HashMap<PathBuf, Observed>,
    /// The watch limit was reached; warned once.
    exhausted: bool,
}

enum Watched {
    /// A profile's `.fingerprint/`, where new unit directories appear.
    Fingerprints { target: PathBuf, dir: PathBuf },
    /// A unit directory, `key` relative to its target.
    Unit { target: PathBuf, key: String },
}

impl Reads {
    pub fn start() -> nix::Result<Self> {
        let shared = Arc::new(Shared {
            inotify: Inotify::init(InitFlags::IN_CLOEXEC)?,
            state: Mutex::default(),
        });
        let reader = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("inotify".into())
            .spawn(move || reader.run())
            .map_err(|e| Errno::from_raw(e.raw_os_error().unwrap_or(0)))?;
        Ok(Self { shared })
    }

    /// Watches every unit of `targets`, and the units they get later. Watches on directories
    /// that are gone end by themselves.
    pub fn watch<'a>(&self, targets: impl IntoIterator<Item = &'a Path>) {
        for target in targets {
            for profile in cargo::profiles(target) {
                // Locked a profile at a time, so events keep flowing meanwhile: the kernel
                // queue holds only so many.
                let mut state = self.shared.lock();
                let fingerprints = profile.join(".fingerprint");
                let watched = Watched::Fingerprints {
                    target: target.to_path_buf(),
                    dir: fingerprints.clone(),
                };
                if !self.shared.add(&mut state, &fingerprints, watched) {
                    return;
                }
                for unit in cargo::unit_dirs(&profile) {
                    if !self.shared.add_unit(&mut state, target, &unit) {
                        return;
                    }
                }
            }
        }
    }

    /// The reads seen in `target` since the daemon started.
    pub fn observed(&self, target: &Path) -> Observed {
        self.shared
            .lock()
            .reads
            .get(target)
            .cloned()
            .unwrap_or_default()
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn add_unit(&self, state: &mut State, target: &Path, unit: &Path) -> bool {
        let watched = Watched::Unit {
            target: target.to_path_buf(),
            key: cargo::unit_key(target, unit),
        };
        self.add(state, unit, watched)
    }

    /// Adds a watch, or returns false once the watch limit is reached.
    fn add(&self, state: &mut State, dir: &Path, watched: Watched) -> bool {
        let events = match watched {
            Watched::Fingerprints { .. } => AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MOVED_TO,
            Watched::Unit { .. } => AddWatchFlags::IN_OPEN,
        };
        let flags = events | AddWatchFlags::IN_ONLYDIR | AddWatchFlags::IN_DONT_FOLLOW;
        match self.inotify.add_watch(dir, flags) {
            Ok(wd) => {
                state.watches.insert(wd, watched);
                true
            }
            Err(Errno::ENOSPC) => {
                if !state.exhausted {
                    state.exhausted = true;
                    tracing::warn!(
                        "out of inotify watches: reads of some Cargo units go unseen; raise \
                         fs.inotify.max_user_watches"
                    );
                }
                false
            }
            // Gone already, or not a real directory.
            Err(_) => true,
        }
    }

    fn run(&self) {
        loop {
            let events = match self.inotify.read_events() {
                Ok(events) => events,
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    tracing::warn!("watching Cargo reads stopped: {e}");
                    return;
                }
            };
            let now = SystemTime::now();
            let mut state = self.lock();
            for event in events {
                if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                    tracing::warn!("missed some Cargo reads: the inotify queue overflowed");
                    continue;
                }
                if event.mask.contains(AddWatchFlags::IN_IGNORED) {
                    if let Some(Watched::Unit { target, key }) = state.watches.remove(&event.wd)
                        && let Some(reads) = state.reads.get_mut(&target)
                    {
                        reads.remove(&key);
                    }
                    continue;
                }
                let Some(name) = event.name.as_deref() else {
                    continue;
                };
                let is_dir = event.mask.contains(AddWatchFlags::IN_ISDIR);
                match state.watches.get(&event.wd) {
                    Some(Watched::Fingerprints { target, dir }) if is_dir => {
                        let (target, unit) = (target.clone(), dir.join(name));
                        self.add_unit(&mut state, &target, &unit);
                    }
                    Some(Watched::Unit { target, key })
                        if !is_dir && name.to_str().is_some_and(cargo::is_hash_file_name) =>
                    {
                        let (target, key) = (target.clone(), key.clone());
                        let latest = state
                            .reads
                            .entry(target)
                            .or_default()
                            .entry(key)
                            .or_insert(now);
                        *latest = (*latest).max(now);
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::cargo::fixtures::Workspace;

    fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn cendrillon_reading_and_arming_hash_files_is_not_a_read() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        let target = ws.target();
        // Remembers the hash files' contents.
        cargo::record_reads(&target, &Observed::new()).unwrap();
        // As after a read where access times show it, so that the next pass arms them again.
        for unit in ws.units() {
            for file in std::fs::read_dir(&unit).unwrap() {
                let path = file.unwrap().path();
                let meta = std::fs::metadata(&path).unwrap();
                let later = meta.modified().unwrap() + Duration::from_secs(10);
                filetime::set_file_atime(&path, filetime::FileTime::from_system_time(later))
                    .unwrap();
            }
        }
        let reads = Reads::start().unwrap();
        reads.watch([target.as_path()]);

        cargo::record_reads(&target, &Observed::new()).unwrap();
        cargo::garbage(&target, Duration::ZERO, SystemTime::now(), &Observed::new()).unwrap();
        cargo::remember_hashes(&target).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(reads.observed(&target).is_empty());

        // While Cargo's reads show.
        let fresh = ws.cargo("build");
        assert!(!fresh.contains("Compiling"), "{fresh}");
        assert!(wait_for(|| !reads.observed(&target).is_empty()));
    }

    #[test]
    fn sees_a_fresh_build_read_its_units_and_learns_of_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        let target = ws.target();
        let reads = Reads::start().unwrap();
        reads.watch([target.as_path()]);
        assert!(reads.observed(&target).is_empty());

        let fresh = ws.cargo("build");
        assert!(!fresh.contains("Compiling"), "{fresh}");
        let keys = |units: &[PathBuf]| -> Vec<String> {
            units
                .iter()
                .map(|unit| cargo::unit_key(&target, unit))
                .collect()
        };
        let first = keys(&ws.units());
        assert!(wait_for(|| first
            .iter()
            .all(|key| reads.observed(&target).contains_key(key))));

        // New units, compiled after the watch began, are watched too.
        ws.enable_extra();
        ws.cargo("build");
        let fresh = ws.cargo("build");
        assert!(!fresh.contains("Compiling"), "{fresh}");
        let all = keys(&ws.units());
        assert!(all.len() > first.len());
        assert!(wait_for(|| all
            .iter()
            .all(|key| reads.observed(&target).contains_key(key))));
    }
}
