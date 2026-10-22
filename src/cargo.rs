//! Cargo target directories: recognizing them, and telling the build units still in use from
//! the ones nothing current depends on.
//!
//! Cargo never deletes a build unit. Every change of features, flags, toolchain or dependency
//! versions compiles new generations of the affected crates next to the old ones. Each unit's
//! `.fingerprint/<unit>/<role>.json` lists the fingerprints of the units it was built against,
//! which gives the unit graph. In use are the newest generation of every top-level unit, the
//! units Cargo read or compiled recently, and everything those depend on.
//!
//! A Fresh build writes nothing, but Cargo reads the fingerprint hash file of every unit in the
//! build, and macOS records a read in the access time while that time is older than the
//! modification time. [`record_reads`] notes the units read since its last call, then sets
//! their access times back just before their modification times, so the next read shows too.
//! Linux does the same with the default `relatime` mount option, but records no read at all
//! with `noatime`, so there the daemon also watches the hash files being opened (see
//! `inotify.rs`) and passes what it saw to [`record_reads`].
//!
//! Since opening a hash file looks like a read to that watcher, cendrillon remembers their
//! contents in the read log and opens one again only after Cargo rewrote it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, FileTimes};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::sys;

/// When cendrillon saw Cargo read each unit. It lives in the target, so it goes with it.
const READ_LOG: &str = ".cendrillon-reads.json";

/// A Cargo target directory, `CARGO_TARGET_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub root: PathBuf,
    /// When Cargo last compiled, built (a build rewrites the dep-info of its uplifted outputs)
    /// or, as of cendrillon's last pass, read anything here.
    pub last_used: SystemTime,
}

impl Target {
    /// Recognizes a target directory: Cargo's `.rustc_info.json` next to at least one profile
    /// directory holding `.fingerprint/` and `deps/`.
    pub fn classify(dir: &Path) -> Option<Self> {
        if !is_file(&dir.join(".rustc_info.json")) {
            return None;
        }
        let profiles = profiles(dir);
        if profiles.is_empty() {
            return None;
        }
        // A log cendrillon cannot read hides when Cargo last used the target: assume now.
        let logged = match ReadLog::load(dir) {
            Ok(log) => log.latest(),
            Err(_) => Some(SystemTime::now()),
        };
        let units = profiles
            .iter()
            .flat_map(|profile| real_subdirs(&profile.join(".fingerprint")))
            .flat_map(|unit| hash_files(&unit))
            .filter_map(|hash_file| {
                let meta = fs::symlink_metadata(hash_file).ok()?;
                let (accessed, modified) = (meta.accessed().ok()?, meta.modified().ok()?);
                Some(accessed.max(modified))
            })
            .chain(logged)
            .max();
        // A target without units only has its directories to go by.
        let last_used =
            units.or_else(|| profiles.iter().filter_map(|profile| mtime(profile)).max());
        Some(Self {
            root: dir.to_path_buf(),
            last_used: last_used?,
        })
    }
}

/// Profile directories of a target: `<target>/<profile>`, `<target>/<triple>/<profile>` for
/// cross builds, and the same inside a nested target dir such as rust-analyzer's
/// `<target>/rust-analyzer`.
pub fn profiles(target: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for child in real_subdirs(target) {
        if is_profile(&child) {
            found.push(child);
            continue;
        }
        let nested_target = is_file(&child.join(".rustc_info.json"));
        for grandchild in real_subdirs(&child) {
            if is_profile(&grandchild) {
                found.push(grandchild);
            } else if nested_target {
                found.extend(
                    real_subdirs(&grandchild)
                        .into_iter()
                        .filter(|dir| is_profile(dir)),
                );
            }
        }
    }
    found.sort();
    found
}

/// The unit directories of a profile: `<profile>/.fingerprint/<package>-<hash>`.
#[cfg(target_os = "linux")]
pub fn unit_dirs(profile: &Path) -> Vec<PathBuf> {
    real_subdirs(&profile.join(".fingerprint"))
}

/// How the read log names a unit directory: relative to its target.
#[cfg(target_os = "linux")]
pub fn unit_key(target: &Path, unit: &Path) -> String {
    ReadLog::key(target, unit)
}

/// Whether a file in a unit directory is a hash file, `<role>`, rather than its `<role>.json`,
/// its dep-info `dep-<role>` or its saved output `output-<role>`.
#[cfg(target_os = "linux")]
pub fn is_hash_file_name(name: &str) -> bool {
    !name.ends_with(".json") && ROLE_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

fn is_profile(dir: &Path) -> bool {
    is_dir(&dir.join(".fingerprint")) && is_dir(&dir.join("deps"))
}

/// Reads of hash files seen as they happened rather than in access times: by unit directory
/// relative to the target, as in the read log, the latest one.
pub type Observed = HashMap<String, SystemTime>;

/// Notes which units Cargo read since the last call, from access times and from `observed`,
/// and sets their hash files up to show the next read. Only access times change: Cargo goes
/// by contents and modification times.
pub fn record_reads(target: &Path, observed: &Observed) -> io::Result<()> {
    let mut log = ReadLog::load(target)?;
    let mut present = HashSet::new();
    let mut hashes = HashSet::new();
    let mut to_arm = Vec::new();
    for profile in profiles(target) {
        for unit in real_subdirs(&profile.join(".fingerprint")) {
            let key = ReadLog::key(target, &unit);
            for hash_file in hash_files(&unit) {
                let meta = fs::symlink_metadata(&hash_file)?;
                let (accessed, modified) = (meta.accessed()?, meta.modified()?);
                if accessed > modified {
                    log.record(&key, accessed);
                }
                log.remember_hash(target, &hash_file, modified, accessed);
                hashes.insert(ReadLog::key(target, &hash_file));
                if accessed >= modified {
                    to_arm.push((hash_file, modified));
                }
            }
            if let Some(&read) = observed.get(&key) {
                log.record(&key, read);
            }
            present.insert(key);
        }
    }
    log.units.retain(|key, _| present.contains(key));
    log.hashes.retain(|key, _| hashes.contains(key));
    // Saved before any access time is reset, so a failure below loses no read.
    log.save(target)?;
    for (hash_file, modified) in to_arm {
        let armed = modified
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        // A file that cannot be armed (built by root, say) only stops showing reads.
        let _ = sys::set_accessed(&hash_file, armed);
    }
    Ok(())
}

/// Remembers the contents of every hash file in `target`, as [`record_reads`] does, but
/// changes nothing else: for a dry run, whose reads would otherwise look like Cargo's.
pub fn remember_hashes(target: &Path) -> io::Result<()> {
    let mut log = ReadLog::load(target)?;
    let mut hashes = HashSet::new();
    for profile in profiles(target) {
        for unit in real_subdirs(&profile.join(".fingerprint")) {
            for hash_file in hash_files(&unit) {
                let meta = fs::symlink_metadata(&hash_file)?;
                log.remember_hash(target, &hash_file, meta.modified()?, meta.accessed()?);
                hashes.insert(ReadLog::key(target, &hash_file));
            }
        }
    }
    log.hashes.retain(|key, _| hashes.contains(key));
    log.save(target)
}

/// The fingerprint hash file of each role of a unit: `<role>` next to `<role>.json`.
fn hash_files(unit: &Path) -> Vec<PathBuf> {
    entries(unit)
        .into_iter()
        .filter_map(|json| {
            let role = json.file_name()?.to_str()?.strip_suffix(".json")?;
            let hash_file = unit.join(role);
            is_file(&hash_file).then_some(hash_file)
        })
        .collect()
}

/// Per unit directory, relative to the target, the last read cendrillon saw, in seconds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ReadLog {
    units: BTreeMap<String, u64>,
    /// Per hash file, relative to the target, its contents as of a modification time.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    hashes: BTreeMap<String, CachedHash>,
}

/// A hash file's contents, and its modification time then, in seconds and nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedHash(u64, u32, String);

impl CachedHash {
    fn new(modified: SystemTime, hash: String) -> Self {
        let since = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        Self(since.as_secs(), since.subsec_nanos(), hash)
    }

    /// The contents, if the file was not rewritten since.
    fn get(&self, modified: SystemTime) -> Option<&str> {
        let current = Self::new(modified, String::new());
        ((self.0, self.1) == (current.0, current.1)).then_some(self.2.as_str())
    }
}

impl ReadLog {
    /// The log, empty if there is none yet. An unreadable one is an error: without it,
    /// generations still in use would look unused.
    fn load(target: &Path) -> io::Result<Self> {
        match fs::read(target.join(READ_LOG)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    fn save(&self, target: &Path) -> io::Result<()> {
        use std::io::Write;
        let temp = target.join(format!("{READ_LOG}.tmp"));
        let mut file = fs::File::create(&temp)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        fs::rename(temp, target.join(READ_LOG))
    }

    fn key(target: &Path, unit: &Path) -> String {
        unit.strip_prefix(target)
            .unwrap_or(unit)
            .to_string_lossy()
            .into_owned()
    }

    fn record(&mut self, key: &str, time: SystemTime) {
        // An access time in the future (a restored backup) would pin the unit until then.
        let secs = time
            .min(SystemTime::now())
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = self.units.entry(key.to_owned()).or_default();
        *entry = (*entry).max(secs);
    }

    /// Reads `hash_file` into the log, unless the log has it as of `modified`. A file that
    /// cannot be read is left out: the trim fails on it later.
    fn remember_hash(
        &mut self,
        target: &Path,
        hash_file: &Path,
        modified: SystemTime,
        accessed: SystemTime,
    ) {
        let key = Self::key(target, hash_file);
        if self
            .hashes
            .get(&key)
            .is_some_and(|cached| cached.get(modified).is_some())
        {
            return;
        }
        if let Ok(hash) = read_quietly(hash_file, accessed) {
            self.hashes.insert(key, CachedHash::new(modified, hash));
        }
    }

    fn get(&self, key: &str) -> Option<SystemTime> {
        let secs = *self.units.get(key)?;
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn latest(&self) -> Option<SystemTime> {
        let secs = *self.units.values().max()?;
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }
}

/// Build output nothing current depends on: what trimming a target deletes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Garbage {
    /// Everything to delete, each inside a profile's `.fingerprint/`, `build/`, `deps/`,
    /// `examples/` or `incremental/`. Lock files and uplifted outputs are never included.
    pub paths: Vec<PathBuf>,
    pub units: usize,
    pub incremental_dirs: usize,
}

/// Finds the garbage in `target`: units neither used within `unit_idle` of `now`, nor the
/// newest generation of a top-level unit, nor needed by one of those. Reads in `observed`
/// count as well as those in the read log.
///
/// Reads fingerprint files and the first bytes of uplifted outputs, and lists directories;
/// writes nothing. Fails rather than guess when a unit cannot be read, since what it depends
/// on is unknown.
pub fn garbage(
    target: &Path,
    unit_idle: Duration,
    now: SystemTime,
    observed: &Observed,
) -> io::Result<Garbage> {
    let recent = now.checked_sub(unit_idle).unwrap_or(SystemTime::UNIX_EPOCH);
    let profiles = profiles(target);
    let log = ReadLog::load(target)?;
    let mut graph = Graph::default();
    for (index, profile) in profiles.iter().enumerate() {
        graph.load(index, profile, target, &log, observed)?;
    }
    let unreadable = graph.dirs.iter().filter(|dir| dir.unknown).count();
    if unreadable > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{unreadable} build units have fingerprints cendrillon cannot read"),
        ));
    }
    let uplifted: HashSet<usize> = profiles
        .iter()
        .enumerate()
        .flat_map(|(index, profile)| graph.uplifted(index, profile))
        .collect();
    let live = graph.live(recent, &uplifted);

    let mut garbage = Garbage::default();
    for (index, profile) in profiles.iter().enumerate() {
        let dead: HashMap<&str, &str> = graph
            .dirs
            .iter()
            .filter(|dir| dir.profile == index && dir.is_dead(&graph.nodes, &live, recent))
            .map(|dir| (dir.hash(), dir.name.as_str()))
            .collect();
        garbage.units += dead.len();
        let build = profile.join("build");
        for name in dead.values() {
            garbage.paths.push(profile.join(".fingerprint").join(name));
            if is_dir(&build) && fs::symlink_metadata(build.join(name)).is_ok() {
                garbage.paths.push(build.join(name));
            }
        }
        for sub in ["deps", "examples"] {
            for entry in entries(&profile.join(sub)) {
                if unit_hash(&entry).is_some_and(|hash| dead.contains_key(hash)) {
                    garbage.paths.push(entry);
                }
            }
        }

        let live_crates = graph.live_crates(index, &live);
        for (dir, used) in dead_incremental_dirs(&profile.join("incremental"), &live_crates) {
            if used < recent {
                garbage.incremental_dirs += 1;
                garbage.paths.push(dir);
            }
        }
    }
    Ok(garbage)
}

/// The unit hash in an output's name: `lib<crate>-<hash>.rlib`, `<crate>-<hash>.<cgu>.rcgu.o`,
/// `<crate>-<hash>.dSYM`, `<crate>-<hash>`.
fn unit_hash(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let stem = name.split('.').next()?;
    let (_, hash) = stem.rsplit_once('-')?;
    is_unit_hash(hash).then_some(hash)
}

/// rustc keeps one incremental directory per compiled unit of a local crate, named after the
/// crate but not the unit. Keeps the most recently written ones of each crate, as many as it
/// has live units and at least one, and returns the others with the time they were last
/// written. A wrong pick costs one non-incremental compile of that crate.
fn dead_incremental_dirs(
    incremental: &Path,
    live_crates: &HashMap<&str, usize>,
) -> Vec<(PathBuf, SystemTime)> {
    let mut by_crate: BTreeMap<String, Vec<(SystemTime, PathBuf)>> = BTreeMap::new();
    for dir in real_subdirs(incremental) {
        let Some(krate) = dir
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.rsplit_once('-'))
            .map(|(krate, _)| krate.to_owned())
        else {
            continue;
        };
        let used = entries(&dir)
            .iter()
            .chain([&dir])
            .filter_map(|path| mtime(path))
            .max();
        if let Some(used) = used {
            by_crate.entry(krate).or_default().push((used, dir));
        }
    }
    let mut dead = Vec::new();
    for (krate, mut dirs) in by_crate {
        dirs.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
        let keep = live_crates.get(krate.as_str()).copied().unwrap_or(0).max(1);
        dead.extend(dirs.into_iter().skip(keep).map(|(used, dir)| (dir, used)));
    }
    dead
}

/// The parts of `<role>.json` that identify a unit and its dependencies.
#[derive(Deserialize)]
struct FingerprintJson {
    target: u64,
    profile: u64,
    path: u64,
    compile_kind: u64,
    /// `[package hash, extern crate name, public, fingerprint]`.
    deps: Vec<(u64, String, bool, u64)>,
}

/// Units with the same key are generations of the same thing: same role (`lib-serde`,
/// `test-lib-api`, `bin-neoctl`, …), target, profile (which tells `check` from `build`), source
/// path and compile kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UnitKey {
    role: String,
    target: u64,
    profile: u64,
    path: u64,
    compile_kind: u64,
}

/// One role of a unit.
struct Node {
    dir: usize,
    key: UnitKey,
    /// The value dependents record for it: the role's hash file, read as little-endian. Cargo
    /// empties the file when the unit failed to compile, so nothing can depend on it.
    fingerprint: Option<u64>,
    /// The crate's name as rustc and `--extern` know it, with `_` for `-`.
    crate_name: String,
    /// When Cargo last compiled the unit (it rewrites the hash file) or read it.
    last_used: SystemTime,
    /// `(package hash, extern crate name, fingerprint)` of each dependency.
    deps: Vec<(u64, String, u64)>,
    /// Running a build script compiles nothing, so it has no incremental directory.
    compiles: bool,
}

/// A `.fingerprint/<package>-<hash>` directory. Its hash also names the unit's files in
/// `deps/`, `build/` and `examples/`.
struct UnitDir {
    profile: usize,
    name: String,
    nodes: Vec<usize>,
    /// Some role in it could not be read or parsed.
    unknown: bool,
}

impl UnitDir {
    fn hash(&self) -> &str {
        self.name.rsplit_once('-').map_or("", |(_, hash)| hash)
    }

    fn is_dead(&self, nodes: &[Node], live: &HashSet<usize>, recent: SystemTime) -> bool {
        !self.unknown
            && !self.nodes.is_empty()
            && self
                .nodes
                .iter()
                .all(|&node| !live.contains(&node) && nodes[node].last_used < recent)
    }
}

#[derive(Default)]
struct Graph {
    nodes: Vec<Node>,
    dirs: Vec<UnitDir>,
}

impl Graph {
    /// Loads a profile's units, with the reads and hash file contents `log` has for them, and
    /// the reads in `observed`.
    fn load(
        &mut self,
        profile_index: usize,
        profile: &Path,
        target: &Path,
        log: &ReadLog,
        observed: &Observed,
    ) -> io::Result<()> {
        for entry in fs::read_dir(profile.join(".fingerprint"))? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !entry.file_type()?.is_dir() || !is_unit_dir_name(&name) {
                continue;
            }
            let dir = self.dirs.len();
            let key = ReadLog::key(target, &entry.path());
            let logged = log.get(&key).max(observed.get(&key).copied());
            let mut unit = UnitDir {
                profile: profile_index,
                name,
                nodes: Vec::new(),
                unknown: false,
            };
            for file in entries(&entry.path()) {
                let Some(role) = file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix(".json"))
                else {
                    continue;
                };
                let hash_file = entry.path().join(role);
                let cached = log.hashes.get(&ReadLog::key(target, &hash_file));
                match read_node(&file, &hash_file, role, dir, logged, cached) {
                    Some(node) => {
                        unit.nodes.push(self.nodes.len());
                        self.nodes.push(node);
                    }
                    None => unit.unknown = true,
                }
            }
            self.dirs.push(unit);
        }
        Ok(())
    }

    /// Nodes in use: the newest generation of every top-level unit, the units of `uplifted`
    /// directories, every unit used since `recent`, and everything they depend on.
    fn live(&self, recent: SystemTime, uplifted: &HashSet<usize>) -> HashSet<usize> {
        let mut by_fingerprint: HashMap<u64, Vec<usize>> = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if let Some(fingerprint) = node.fingerprint {
                by_fingerprint.entry(fingerprint).or_default().push(index);
            }
        }
        let mut referenced: HashSet<usize> = HashSet::new();
        let mut packages: HashMap<u64, Vec<usize>> = HashMap::new();
        for (package, _, fingerprint) in self.nodes.iter().flat_map(|node| &node.deps) {
            for &dep in by_fingerprint.get(fingerprint).into_iter().flatten() {
                referenced.insert(dep);
                packages.entry(*package).or_default().push(dep);
            }
        }

        let mut newest: HashMap<&UnitKey, usize> = HashMap::new();
        for index in (0..self.nodes.len()).filter(|index| !referenced.contains(index)) {
            let best = newest.entry(&self.nodes[index].key).or_insert(index);
            if self.nodes[*best].last_used < self.nodes[index].last_used {
                *best = index;
            }
        }
        let mut pending: Vec<usize> = newest.into_values().collect();
        pending.extend(uplifted.iter().flat_map(|&dir| &self.dirs[dir].nodes));
        pending
            .extend((0..self.nodes.len()).filter(|&index| self.nodes[index].last_used >= recent));

        let mut live = HashSet::new();
        let mut unresolved: HashSet<(u64, &str)> = HashSet::new();
        let mut handled: HashSet<(u64, &str)> = HashSet::new();
        loop {
            while let Some(index) = pending.pop() {
                if !live.insert(index) {
                    continue;
                }
                for (package, name, fingerprint) in &self.nodes[index].deps {
                    match by_fingerprint.get(fingerprint) {
                        Some(deps) => pending.extend(deps),
                        // Built against a version of this dependency that has been rebuilt
                        // since, so the unit is dirty, and which generation its rebuild will
                        // use is unknown: keep them all, found by package or by crate name.
                        None => {
                            unresolved.insert((*package, name.as_str()));
                        }
                    }
                }
            }
            let new: Vec<(u64, &str)> = unresolved.difference(&handled).copied().collect();
            if new.is_empty() {
                break;
            }
            for (package, name) in new {
                handled.insert((package, name));
                pending.extend(packages.get(&package).into_iter().flatten());
                pending.extend(
                    (0..self.nodes.len()).filter(|&index| self.nodes[index].crate_name == name),
                );
            }
        }
        live
    }

    /// For one profile, how many live compiled units each crate has.
    fn live_crates(&self, profile: usize, live: &HashSet<usize>) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for &index in live {
            let node = &self.nodes[index];
            if node.compiles && self.dirs[node.dir].profile == profile {
                *counts.entry(node.crate_name.as_str()).or_default() += 1;
            }
        }
        counts
    }

    /// Units whose outputs the last build uplifted into the profile directory, as
    /// `<profile>/<name>` and `<profile>/examples/<name>`. A Fresh build uplifts again, so
    /// they name what is in use even when another generation was compiled more recently.
    fn uplifted(&self, profile_index: usize, profile: &Path) -> Vec<usize> {
        // `app` uplifts `deps/app-<hash>`, `libapi.rlib` uplifts `deps/libapi-<hash>.rlib`.
        let split = |path: &Path| -> Option<(String, String)> {
            let name = path.file_name()?.to_str()?;
            let (stem, rest) = name.split_at(name.find('.').unwrap_or(name.len()));
            Some((stem.to_owned(), rest.to_owned()))
        };
        let mut outputs: HashMap<(String, String), PathBuf> = HashMap::new();
        for dir in [profile.to_path_buf(), profile.join("examples")] {
            for entry in entries(&dir) {
                if unit_hash(&entry).is_none() && is_file(&entry) {
                    outputs.extend(split(&entry).map(|name| (name, entry.clone())));
                }
            }
        }
        let by_hash: HashMap<&str, usize> = (0..self.dirs.len())
            .filter(|&dir| self.dirs[dir].profile == profile_index)
            .map(|dir| (self.dirs[dir].hash(), dir))
            .collect();
        let mut uplifted = Vec::new();
        for sub in ["deps", "examples"] {
            for entry in entries(&profile.join(sub)) {
                let Some(hash) = unit_hash(&entry) else {
                    continue;
                };
                let Some(&dir) = by_hash.get(hash) else {
                    continue;
                };
                let Some((stem, rest)) = split(&entry) else {
                    continue;
                };
                let Some(stem) = stem.strip_suffix(&format!("-{hash}")) else {
                    continue;
                };
                if outputs
                    .get(&(stem.to_owned(), rest))
                    .is_some_and(|output| same_contents(output, &entry))
                {
                    uplifted.push(dir);
                }
            }
        }
        uplifted
    }
}

/// Reads one role of a unit. `None` when anything is missing or unexpected, so the unit is
/// not guessed at.
fn read_node(
    json: &Path,
    hash_file: &Path,
    role: &str,
    dir: usize,
    logged: Option<SystemTime>,
    cached: Option<&CachedHash>,
) -> Option<Node> {
    let parsed: FingerprintJson = serde_json::from_slice(&fs::read(json).ok()?).ok()?;
    let (hash, compiled, accessed) = read_hash_file(hash_file, cached).ok()?;
    let fingerprint = match hash.trim() {
        "" => None,
        hex => Some(u64::from_le_bytes(decode_hex(hex)?.try_into().ok()?)),
    };
    // Read since cendrillon last set the access time back.
    let read = Some(accessed).filter(|&accessed| accessed > compiled);
    let last_used = [Some(compiled), read, logged].into_iter().flatten().max()?;
    Some(Node {
        dir,
        key: UnitKey {
            role: role.to_owned(),
            target: parsed.target,
            profile: parsed.profile,
            path: parsed.path,
            compile_kind: parsed.compile_kind,
        },
        fingerprint,
        crate_name: crate_name(role),
        last_used,
        deps: parsed
            .deps
            .into_iter()
            .map(|(package, name, _, fingerprint)| (package, name, fingerprint))
            .collect(),
        compiles: !role.starts_with("run-"),
    })
}

/// A hash file's contents, modification time and access time. The contents come from
/// `cached` when Cargo has not rewritten the file since, so that the file is not opened.
fn read_hash_file(
    path: &Path,
    cached: Option<&CachedHash>,
) -> io::Result<(String, SystemTime, SystemTime)> {
    let meta = fs::symlink_metadata(path)?;
    let (modified, accessed) = (meta.modified()?, meta.accessed()?);
    let hash = match cached.and_then(|cached| cached.get(modified)) {
        Some(hash) => hash.to_owned(),
        None => read_quietly(path, accessed)?,
    };
    Ok((hash, modified, accessed))
}

/// A file's contents. Reading may move the access time, which is how cendrillon tells that
/// Cargo read the file, so it is put back to `accessed`.
fn read_quietly(path: &Path, accessed: SystemTime) -> io::Result<String> {
    use std::io::Read;
    let mut file = sys::open_quietly(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    if file.metadata()?.accessed()? != accessed {
        file.set_times(FileTimes::new().set_accessed(accessed))?;
    }
    Ok(text)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The crate a role compiles: role names are `<kind>-<target name>`, and the target name with
/// `_` for `-` is the crate name.
fn crate_name(role: &str) -> String {
    ROLE_PREFIXES
        .iter()
        .find_map(|prefix| role.strip_prefix(prefix))
        .unwrap_or(role)
        .replace('-', "_")
}

const ROLE_PREFIXES: [&str; 11] = [
    "run-build-script-",
    "build-script-",
    "test-integration-test-",
    "test-example-",
    "test-bench-",
    "test-lib-",
    "test-bin-",
    "example-",
    "bench-",
    "lib-",
    "bin-",
];

/// `<package>-<16 hex>`.
fn is_unit_dir_name(name: &str) -> bool {
    name.rsplit_once('-')
        .is_some_and(|(package, hash)| !package.is_empty() && is_unit_hash(hash))
}

/// Whether `copy` is a copy of `original`: Cargo hard-links uplifted outputs on some systems
/// and clones them on macOS. Different builds differ early in the file (a Mach-O UUID, an
/// rlib's metadata hash), so the length and first 64 KiB tell them apart.
fn same_contents(copy: &Path, original: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::symlink_metadata(copy), fs::symlink_metadata(original)) else {
        return false;
    };
    if (a.dev(), a.ino()) == (b.dev(), b.ino()) {
        return true;
    }
    if !a.is_file() || !b.is_file() || a.len() != b.len() {
        return false;
    }
    let head = |path: &Path| -> Option<Vec<u8>> {
        use std::io::Read;
        let mut buffer = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(64 * 1024)
            .read_to_end(&mut buffer)
            .ok()?;
        Some(buffer)
    };
    head(copy).is_some_and(|copy| head(original) == Some(copy))
}

fn is_unit_hash(hash: &str) -> bool {
    hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Subdirectories, not following symlinks, skipping hidden ones (cendrillon's own trash).
fn real_subdirs(dir: &Path) -> Vec<PathBuf> {
    entries(dir)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !name.starts_with('.'))
                && is_dir(path)
        })
        .collect()
}

/// Entries of `dir`, or none if it is not a real directory: a symlink is never listed, so
/// nothing found through one is ever deleted.
fn entries(dir: &Path) -> Vec<PathBuf> {
    if !is_dir(dir) {
        return Vec::new();
    }
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect()
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
pub mod fixtures {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A workspace with an `app` binary depending on a `dep` library, built by real Cargo.
    pub struct Workspace {
        pub root: PathBuf,
    }

    impl Workspace {
        pub fn new(dir: &Path) -> Self {
            let root = dir.join("ws");
            for (name, manifest, source, file) in [
                (
                    "dep",
                    "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [features]\nextra = []\n",
                    "pub fn value() -> u32 { 1 }\n",
                    "lib.rs",
                ),
                (
                    "app",
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [dependencies]\ndep = { path = \"../dep\" }\n",
                    "fn main() { println!(\"{}\", dep::value()); }\n",
                    "main.rs",
                ),
            ] {
                fs::create_dir_all(root.join(name).join("src")).unwrap();
                fs::write(root.join(name).join("Cargo.toml"), manifest).unwrap();
                fs::write(root.join(name).join("src").join(file), source).unwrap();
            }
            fs::write(
                root.join("Cargo.toml"),
                "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"3\"\n",
            )
            .unwrap();
            Self { root }
        }

        pub fn target(&self) -> PathBuf {
            self.root.join("target")
        }

        /// Enables `dep/extra` from `app`, which makes new generations of both crates.
        pub fn enable_extra(&self) {
            self.edit_app(
                "dep = { path = \"../dep\" }",
                "dep = { path = \"../dep\", features = [\"extra\"] }",
            );
        }

        pub fn disable_extra(&self) {
            self.edit_app(
                "dep = { path = \"../dep\", features = [\"extra\"] }",
                "dep = { path = \"../dep\" }",
            );
        }

        fn edit_app(&self, from: &str, to: &str) {
            let manifest = self.root.join("app/Cargo.toml");
            let text = fs::read_to_string(&manifest).unwrap().replace(from, to);
            fs::write(manifest, text).unwrap();
        }

        /// The unit directories of the debug profile.
        pub fn units(&self) -> Vec<PathBuf> {
            let mut units: Vec<PathBuf> = fs::read_dir(self.target().join("debug/.fingerprint"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            units.sort();
            units
        }

        /// Runs a Cargo command and returns its `-v` output, which says what was Fresh.
        pub fn cargo(&self, command: &str) -> String {
            let output = Command::new("cargo")
                .args([command, "--offline", "-v"])
                .current_dir(&self.root)
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("RUSTFLAGS")
                .env_remove("RUSTC_WORKSPACE_WRAPPER")
                .env("CARGO_INCREMENTAL", "1")
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(output.status.success(), "cargo {command} failed:\n{stderr}");
            stderr
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::Workspace;
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    /// Just after everything built so far.
    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(1)
    }

    fn remove(paths: &[PathBuf]) {
        for path in paths {
            fs::remove_dir_all(path)
                .or_else(|_| fs::remove_file(path))
                .unwrap();
        }
    }

    /// Whether the file system holding `dir` records reads in access times, which Linux does
    /// not when mounted `noatime`.
    fn records_reads(dir: &Path) -> bool {
        let probe = dir.join("atime-probe");
        fs::write(&probe, "probe").unwrap();
        let modified = mtime(&probe).unwrap();
        let before = filetime::FileTime::from_system_time(modified - Duration::from_secs(10));
        filetime::set_file_atime(&probe, before).unwrap();
        fs::read(&probe).unwrap();
        let meta = fs::metadata(&probe).unwrap();
        fs::remove_file(&probe).unwrap();
        let recorded = meta.accessed().unwrap() > meta.modified().unwrap();
        if !recorded {
            eprintln!(
                "skipped: {} records no reads in access times",
                dir.display()
            );
        }
        recorded
    }

    /// Makes every unit look compiled `age` ago.
    fn backdate(units: &[PathBuf], age: Duration) {
        let time = filetime::FileTime::from_system_time(SystemTime::now() - age);
        for unit in units {
            for hash_file in hash_files(unit) {
                filetime::set_file_times(&hash_file, time, time).unwrap();
            }
        }
    }

    #[test]
    fn recognizes_a_target_and_nothing_that_merely_resembles_one() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        let target = Target::classify(&ws.target()).unwrap();
        assert_eq!(profiles(&target.root), [ws.target().join("debug")]);
        assert!(target.last_used.elapsed().unwrap() < Duration::from_secs(600));

        let fake = dir.path().join("fake");
        fs::create_dir_all(fake.join("debug/.fingerprint")).unwrap();
        fs::create_dir_all(fake.join("debug/deps")).unwrap();
        assert_eq!(Target::classify(&fake), None);
        fs::write(fake.join(".rustc_info.json"), "{}").unwrap();
        assert!(Target::classify(&fake).is_some());
        fs::remove_dir_all(fake.join("debug/deps")).unwrap();
        assert_eq!(Target::classify(&fake), None);
    }

    #[test]
    fn superseded_generations_are_garbage_and_the_next_builds_are_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.cargo("check");
        let none = garbage(&ws.target(), Duration::ZERO, later(), &Observed::new()).unwrap();
        assert_eq!(none, Garbage::default());

        ws.enable_extra();
        ws.cargo("build");
        ws.cargo("check");
        assert_eq!(ws.units().len(), 8);

        // Everything was compiled just now.
        let recent = garbage(&ws.target(), DAY, SystemTime::now(), &Observed::new()).unwrap();
        assert_eq!(recent.units, 0);

        // Past `unit_idle`, the first build's and the first check's app and dep go.
        let found = garbage(&ws.target(), Duration::ZERO, later(), &Observed::new()).unwrap();
        assert_eq!(found.units, 4, "{found:#?}");
        assert_eq!(found.incremental_dirs, 4, "{found:#?}");
        let debug = ws.target().join("debug");
        assert!(found.paths.iter().all(|path| path.starts_with(&debug)));
        remove(&found.paths);

        let build = ws.cargo("build");
        assert!(!build.contains("Compiling"), "{build}");
        let check = ws.cargo("check");
        assert!(!check.contains("Checking"), "{check}");
    }

    #[test]
    fn an_older_generation_cargo_still_reads_stays() {
        let dir = tempfile::tempdir().unwrap();
        if !records_reads(dir.path()) {
            return;
        }
        let ws = Workspace::new(dir.path());
        ws.cargo("check");
        let first = ws.units();
        ws.enable_extra();
        ws.cargo("check");
        let second: Vec<PathBuf> = ws
            .units()
            .into_iter()
            .filter(|unit| !first.contains(unit))
            .collect();
        ws.disable_extra();
        backdate(&first, 3 * DAY);
        backdate(&second, 2 * DAY);
        record_reads(&ws.target(), &Observed::new()).unwrap();

        // Nobody read the first generation since it was compiled three days ago.
        assert_eq!(
            garbage(&ws.target(), DAY, SystemTime::now(), &Observed::new())
                .unwrap()
                .units,
            2
        );

        // A Fresh check reads it, as rust-analyzer's does on every save. Now the second
        // generation is the one nobody used for two days.
        let check = ws.cargo("check");
        assert!(!check.contains("Checking"), "{check}");
        for found in [
            garbage(&ws.target(), DAY, SystemTime::now(), &Observed::new()).unwrap(),
            {
                record_reads(&ws.target(), &Observed::new()).unwrap();
                garbage(&ws.target(), DAY, SystemTime::now(), &Observed::new()).unwrap()
            },
        ] {
            assert_eq!(found.units, 2, "{found:#?}");
            let unit_paths: Vec<_> = found
                .paths
                .iter()
                .filter(|path| {
                    path.parent()
                        .is_some_and(|dir| dir.ends_with(".fingerprint"))
                })
                .collect();
            assert!(
                unit_paths.iter().all(|path| second.contains(path)),
                "{found:#?}"
            );
        }
        assert!(
            Target::classify(&ws.target())
                .unwrap()
                .last_used
                .elapsed()
                .unwrap()
                < Duration::from_secs(600)
        );
    }

    #[test]
    fn record_reads_sets_hash_files_up_to_show_the_next_read() {
        let dir = tempfile::tempdir().unwrap();
        if !records_reads(dir.path()) {
            return;
        }
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        let read_since_armed = || -> usize {
            ws.units()
                .iter()
                .flat_map(|unit| hash_files(unit))
                .filter(|file| {
                    let meta = fs::metadata(file).unwrap();
                    meta.accessed().unwrap() > meta.modified().unwrap()
                })
                .count()
        };
        record_reads(&ws.target(), &Observed::new()).unwrap();
        let mtimes: Vec<_> = ws
            .units()
            .iter()
            .flat_map(|unit| hash_files(unit))
            .map(|f| mtime(&f))
            .collect();
        assert_eq!(read_since_armed(), 0);

        ws.cargo("build");
        assert_eq!(read_since_armed(), 2);
        record_reads(&ws.target(), &Observed::new()).unwrap();
        assert_eq!(read_since_armed(), 0);
        assert_eq!(ReadLog::load(&ws.target()).unwrap().units.len(), 2);
        let after: Vec<_> = ws
            .units()
            .iter()
            .flat_map(|unit| hash_files(unit))
            .map(|f| mtime(&f))
            .collect();
        assert_eq!(mtimes, after, "modification times must not change");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn an_older_generation_cargo_still_reads_stays_when_watched() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        let target = ws.target();
        ws.cargo("check");
        let first = ws.units();
        ws.enable_extra();
        ws.cargo("check");
        let second: Vec<PathBuf> = ws
            .units()
            .into_iter()
            .filter(|unit| !first.contains(unit))
            .collect();
        ws.disable_extra();
        backdate(&first, 3 * DAY);
        backdate(&second, 2 * DAY);
        // Remembers every hash file, so cendrillon opens none from now on.
        record_reads(&target, &Observed::new()).unwrap();
        let reads = crate::inotify::Reads::start().unwrap();
        reads.watch([target.as_path()]);

        let found = garbage(&target, DAY, SystemTime::now(), &Observed::new()).unwrap();
        assert_eq!(found.units, 2, "{found:#?}");
        assert!(reads.observed(&target).is_empty());

        // A Fresh check reads the first generation: now the second is the one nobody used.
        let check = ws.cargo("check");
        assert!(!check.contains("Checking"), "{check}");
        let keys = |units: &[PathBuf]| -> HashSet<String> {
            units.iter().map(|unit| unit_key(&target, unit)).collect()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !keys(&first)
            .iter()
            .all(|key| reads.observed(&target).contains_key(key))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the reads went unseen"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        record_reads(&target, &reads.observed(&target)).unwrap();
        let found = garbage(&target, DAY, SystemTime::now(), &Observed::new()).unwrap();
        assert_eq!(found.units, 2, "{found:#?}");
        assert!(
            found
                .paths
                .iter()
                .filter(|path| path
                    .parent()
                    .is_some_and(|dir| dir.ends_with(".fingerprint")))
                .all(|path| second.contains(path)),
            "{found:#?}"
        );
        // cendrillon's own reads, of the remembered hash files, went unseen.
        let seen: HashSet<String> = reads.observed(&target).into_keys().collect();
        assert!(seen.is_disjoint(&keys(&second)), "{seen:?}");
    }

    #[test]
    fn the_uplifted_generation_stays_even_without_access_times() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.enable_extra();
        ws.cargo("build");
        ws.disable_extra();
        let fresh = ws.cargo("build");
        assert!(!fresh.contains("Compiling"), "{fresh}");
        // As on a volume that records no reads.
        for hash_file in ws.units().iter().flat_map(|unit| hash_files(unit)) {
            let modified = mtime(&hash_file).unwrap();
            let before = filetime::FileTime::from_system_time(modified - Duration::from_secs(1));
            filetime::set_file_atime(&hash_file, before).unwrap();
        }

        // The second build compiled last, but the uplifted `app` is the first one's.
        let found = garbage(&ws.target(), Duration::ZERO, later(), &Observed::new()).unwrap();
        assert_eq!(found.units, 0, "{found:#?}");
    }

    #[test]
    fn never_deletes_through_a_symlinked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.enable_extra();
        ws.cargo("build");
        let outside = dir.path().join("outside");
        for name in ["notes-1", "notes-2", "dep-0123456789abcdef"] {
            fs::create_dir_all(outside.join(name)).unwrap();
        }
        let debug = ws.target().join("debug");
        for sub in ["incremental", "examples", "build"] {
            let _ = fs::remove_dir_all(debug.join(sub));
            std::os::unix::fs::symlink(&outside, debug.join(sub)).unwrap();
        }

        let found = garbage(&ws.target(), Duration::ZERO, later(), &Observed::new()).unwrap();
        assert!(found.units > 0);
        let outside = fs::canonicalize(&outside).unwrap();
        for path in &found.paths {
            let parent = fs::canonicalize(path.parent().unwrap()).unwrap();
            assert!(!parent.starts_with(&outside), "{}", path.display());
        }
    }

    #[test]
    fn refuses_to_trim_when_a_unit_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.enable_extra();
        ws.cargo("build");
        for file in entries(&ws.units()[0]) {
            if file.extension().is_some_and(|ext| ext == "json") {
                fs::write(&file, "not json").unwrap();
            }
        }
        assert!(garbage(&ws.target(), Duration::ZERO, later(), &Observed::new()).is_err());
    }

    #[test]
    fn a_corrupt_read_log_stops_trimming_and_keeps_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        fs::write(ws.target().join(READ_LOG), "{").unwrap();
        assert!(record_reads(&ws.target(), &Observed::new()).is_err());
        assert!(garbage(&ws.target(), Duration::ZERO, later(), &Observed::new()).is_err());
        let target = Target::classify(&ws.target()).unwrap();
        assert!(target.last_used.elapsed().unwrap_or_default() < Duration::from_secs(60));
    }

    #[test]
    fn crate_names_come_from_role_names() {
        assert_eq!(crate_name("lib-alass_core"), "alass_core");
        assert_eq!(crate_name("test-integration-test-my-test"), "my_test");
        assert_eq!(crate_name("bin-my-tool"), "my_tool");
        assert_eq!(
            crate_name("build-script-build-script-build"),
            "build_script_build"
        );
        assert_eq!(
            crate_name("run-build-script-build-script-build"),
            "build_script_build"
        );
    }

    #[test]
    fn decodes_fingerprints_as_cargo_writes_them() {
        assert_eq!(
            u64::from_le_bytes(decode_hex("7c1e9a5aa56ea158").unwrap().try_into().unwrap()),
            6386507403056782972
        );
        assert_eq!(decode_hex("abc"), None);
        assert!(is_unit_dir_name("alass-core-67efb9fd379c5457"));
        assert!(!is_unit_dir_name("notes-2026"));
    }
}
