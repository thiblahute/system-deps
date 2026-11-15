use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    fs, io,
    path::{Path, PathBuf},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime},
};

use heck::ToShoutySnakeCase;
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::error::{BinaryError, Error};

/// The extension of the binary archive.
/// Support for different extensions is enabled using features.
#[derive(Debug)]
pub enum Extension {
    /// A `.tar.gz` archive.
    #[cfg(feature = "gz")]
    TarGz,
    /// A `.tar.xz` archive.
    #[cfg(feature = "xz")]
    TarXz,
    /// A `.zip` archive.
    #[cfg(feature = "zip")]
    Zip,
    /// A macOS `.pkg` (Apple flat package) archive.
    #[cfg(all(feature = "pkg", target_os = "macos"))]
    Pkg,
    /// A Windows Inno Setup `.exe` installer.
    #[cfg(all(feature = "inno", target_os = "windows"))]
    Inno,
    Folder,
}

impl TryFrom<&str> for Extension {
    type Error = BinaryError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Path::new(value).try_into()
    }
}

impl TryFrom<&Path> for Extension {
    type Error = BinaryError;
    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        if path.is_dir() {
            return Ok(Self::Folder);
        };
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("<none>");
        match extension {
            #[cfg(all(feature = "pkg", target_os = "macos"))]
            "pkg" => Ok(Extension::Pkg),
            #[cfg(all(feature = "inno", target_os = "windows"))]
            "exe" => Ok(Extension::Inno),
            #[cfg(feature = "gz")]
            "gz" | "tgz" => Ok(Extension::TarGz),
            #[cfg(feature = "xz")]
            "xz" => Ok(Extension::TarXz),
            #[cfg(feature = "zip")]
            "zip" => Ok(Extension::Zip),
            e => Err(BinaryError::UnsupportedExtension(e.into())),
        }
    }
}

/// Binary locations can be specified either by describing its metadata or by refering to another
/// package. This helper enum allows deserializing both as valid versions.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Binary {
    Follow(FollowBinary),
    Url(UrlBinary),
}

impl TryFrom<Value> for Binary {
    type Error = Error;
    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Ok(value.try_into()?)
    }
}

/// A package that doesn't point to a binary itself but instead uses the metadata from another one.
/// While the `follows` field can be specified, it is usually more convenient to use `provides` in
/// the package that defines the binary url. Both of these are equivalent:
///
/// ```toml
/// [package.metadata.system-deps.a]
/// url = "..."
/// provides = [ "b" ]
///
/// [package.metadata.system-deps.b]
/// follows = "a"
/// ```
///
/// Specifying both an url and a followed package is incompatible and it will cause an error.
#[derive(Debug, Deserialize)]
pub struct FollowBinary {
    /// The package name to get the metadata from.
    follows: String,
}

/// Represents one location from where to download prebuilt binaries.
#[derive(Debug, Deserialize)]
pub struct UrlBinary {
    /// The url from which to download the archived binaries. It suppports:
    ///
    /// - Web urls, in the form `http[s]://website/archive.ext`.
    ///   This must directly download an archive with a known `Extension`.
    /// - Local files, in the form `file:///path/to/archive.ext`.
    ///   Note that this is made of the url descriptor `file://`, and then an absolute path, that
    ///   starts with `/`, so three total slashes are needed.
    ///   The path can point at an archive with a known `Extension`, or to a folder containing the
    ///   uncompressed binaries.
    url: String,
    /// Optionally, a checksum of the downloaded archive. When set, it is used to correctly cache
    /// the result. If this is not specified, it will still be cached by cargo, but redownloads
    /// might happen more often. It has no effect if `url` is a local folder.
    checksum: Option<String>,
    /// A list of relative paths inside the binary archive that point to a folder containing
    /// package config files (`.pc`). These directories will be prepended to the `PKG_CONFIG_PATH` when
    /// compiling the affected libraries.
    paths: Option<Vec<String>>,
}

/// What the build scripts of one build share: the paths of the prebuilt binaries, and the
/// environment variables that were looked at to decide them.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct CachedPaths {
    /// Cargo must be told about these by every build script, also by those that read the cache.
    pub vars: Vec<String>,
    /// The paths.
    pub paths: Paths,
}

/// A key for [`Paths::cached`]: a hash of everything the paths depend on, given as `inputs`.
pub fn cache_key(inputs: &[&[u8]]) -> String {
    let mut all = Vec::new();
    for input in inputs {
        // Each is preceded by its length, so that moving a byte between two is another key.
        all.extend_from_slice(&(input.len() as u64).to_le_bytes());
        all.extend_from_slice(input);
    }
    sha256::digest(&*all)
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Paths {
    paths: HashMap<String, Vec<PathBuf>>,
    follows: HashMap<String, String>,
    wildcards: HashMap<String, String>,
}

impl Paths {
    /// Uses the metadata from the cargo manifests and the environment to build a list of urls
    /// from where to download binaries for dependencies and adds them to their `PKG_CONFIG_PATH`.
    pub fn from_binaries<T>(binaries: impl IntoIterator<Item = (String, T)>) -> Result<Self, Error>
    where
        Binary: TryFrom<T>,
    {
        let mut res = Self::default();
        let mut auto_detect = std::collections::HashSet::new();

        let (url_binaries, follow_binaries): (Vec<_>, Vec<_>) = binaries
            .into_iter()
            .filter_map(|(k, v)| Some((k, v.try_into().ok()?)))
            .partition(|(_, bin)| matches!(bin, Binary::Url(_)));

        // Binaries with its own url
        let errors: Vec<BinaryError> = thread::scope(|s| {
            let mut handles = Vec::new();

            for (name, bin) in url_binaries {
                let Binary::Url(bin) = bin else {
                    unreachable!();
                };

                let dst = Path::new(&crate::TARGET_DIR).join(&name);
                if let Some(ref paths) = bin.paths {
                    res.paths
                        .insert(name, paths.iter().map(|p| dst.join(p)).collect());
                } else {
                    auto_detect.insert(name.clone());
                    res.paths.insert(name, Vec::new());
                }

                // Only refresh the binaries if there isn't already a valid copy
                let valid = check_valid_dir(&dst, bin.checksum.as_deref())?;

                // Allow multiple downloads at the same time
                if !valid {
                    handles.push(s.spawn(move || make_available_once(bin, &dst)));
                }
            }

            Ok::<_, BinaryError>(
                handles
                    .into_iter()
                    .filter_map(|h| h.join().expect("download thread panicked").err())
                    .collect(),
            )
        })?;

        if let Some(e) = errors.into_iter().next() {
            return Err(e.into());
        }

        // Auto-detect pkgconfig directories for packages that didn't specify paths
        for name in &auto_detect {
            let dst = Path::new(&crate::TARGET_DIR).join(name);
            if let Some(list) = res.paths.get_mut(name) {
                *list = find_pkgconfig_dirs(&dst);
            }
        }

        // Check if the package provided extra configuration
        for (name, list) in res.paths.iter_mut() {
            let dst = Path::new(&crate::TARGET_DIR).join(name);
            let Ok(info) = fs::read_to_string(dst.join("info.toml")) else {
                continue;
            };
            let Ok(table) = toml::from_str::<Table>(&info) else {
                continue;
            };
            if let Some(Value::Array(paths)) = table.get("paths") {
                for p in paths.iter().filter_map(|p| p.as_str()) {
                    let p = dst.join(p);
                    if !list.contains(&p) {
                        list.push(p);
                    }
                }
            }
        }

        // Binaries that follow others
        for (name, bin) in follow_binaries {
            let Binary::Follow(bin) = bin else {
                unreachable!();
            };
            if !res.paths.contains_key(&bin.follows) {
                return Err(BinaryError::InvalidFollows(name, bin.follows).into());
            };
            match name.strip_suffix("*") {
                Some(wildcard) => res.wildcards.insert(wildcard.into(), bin.follows),
                None => res.follows.insert(name, bin.follows),
            };
        }

        Ok(res)
    }
}

impl Paths {
    /// Returns the list of paths for a certain package. Matches wildcards but they never have
    /// priority over explicit urls or follows, even if they are defined higher in the hierarchy.
    pub fn get(&self, key: &str) -> Option<&Vec<PathBuf>> {
        if let Some(paths) = self.paths.get(key) {
            return Some(paths);
        };

        if let Some(follows) = self.follows.get(key) {
            return self.paths.get(follows);
        };

        self.wildcards.iter().find_map(|(k, v)| {
            key.starts_with(k)
                .then_some(v)
                .and_then(|v| self.paths.get(v))
        })
    }

    /// Whether every directory listed still exists.
    fn all_exist(&self) -> bool {
        self.paths.values().flatten().all(|p| p.exists())
    }

    /// The paths for `key`, worked out by the first caller and read from a file by the others.
    ///
    /// Working them out reads the whole crate graph, and every build script that uses
    /// `system-deps` needs them. The first one takes a lock and does it (waiting while another one
    /// is doing it); the rest, in this build and in the next ones, read the result. `key` must
    /// cover everything the result depends on: see [`cache_key`].
    pub fn cached(
        key: &str,
        compute: impl FnOnce() -> Result<CachedPaths, Error>,
    ) -> Result<CachedPaths, Error> {
        let file = Path::new(&crate::TARGET_DIR).join(format!("paths-{key}.toml"));
        // A copy whose directories are gone (a cleaned target directory) is not worth reading.
        let read = || -> Option<CachedPaths> {
            let cached: CachedPaths = toml::from_str(&fs::read_to_string(&file).ok()?).ok()?;
            cached.paths.all_exist().then_some(cached)
        };
        if let Some(cached) = read() {
            return Ok(cached);
        }

        match DirLock::acquire(&file.with_extension(""), || Ok(read().is_some()))? {
            Some(lock) => {
                let result = compute();
                match &result {
                    Ok(cached) => {
                        // Written under another name and renamed, so nobody reads half of it.
                        let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
                        fs::write(&tmp, toml::to_string(cached)?)
                            .and_then(|_| fs::rename(&tmp, &file))
                            .map_err(BinaryError::InvalidDirectory)?;
                    }
                    Err(error) => lock.record_failure(error),
                }
                result
            }
            None => read().ok_or_else(|| {
                BinaryError::InvalidDirectory(io::Error::new(
                    io::ErrorKind::NotFound,
                    "the shared paths disappeared",
                ))
                .into()
            }),
        }
    }

    /// Serializes the path list.
    pub fn to_string(&self) -> Result<String, Error> {
        Ok(toml::to_string(self)?)
    }

    /// First executable `bin/pkg-config` shipped by any downloaded bundle,
    /// or `None`. Used to probe .pc files when the host has no pkg-config.
    pub fn pkg_config_binary(&self) -> Option<PathBuf> {
        let exe = if cfg!(windows) {
            "pkg-config.exe"
        } else {
            "pkg-config"
        };
        self.paths.keys().find_map(|name| {
            let candidate = Path::new(&crate::TARGET_DIR)
                .join(name)
                .join("bin")
                .join(exe);
            which::which(&candidate).ok()
        })
    }

    /// Returns the directories (one per binary install) where runtime DLLs
    /// live. On Windows the test/run binaries cannot find these via
    /// `cargo:rustc-link-search`; the loader looks at `PATH` or the exe's
    /// directory at runtime, so the caller is expected to surface these to
    /// the user.
    #[cfg(windows)]
    pub fn dll_search_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for name in self.paths.keys() {
            let bin = Path::new(&crate::TARGET_DIR).join(name).join("bin");
            if bin.is_dir() && seen.insert(bin.clone()) {
                out.push(bin);
            }
        }
        out
    }
}

/// Iteratively scan `dir` for subdirectories named "pkgconfig" and return their paths.
/// Does not follow symlinks. Limited to `MAX_DEPTH` levels to avoid excessive traversal.
fn find_pkgconfig_dirs(dir: &Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 10;

    let mut result = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Use symlink_metadata (lstat) to avoid following symlinks
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if path.file_name().is_some_and(|n| n == "pkgconfig") {
                    result.push(path);
                } else if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
            }
        }
    }
    result
}

/// Checks if the target directory is valid and if binaries need to be redownloaded.
/// On an `Ok` result, if the value is true it means that the directory is correct.
fn check_valid_dir(dst: &Path, checksum: Option<&str>) -> Result<bool, BinaryError> {
    // If it doesn't exist yet the download will need to happen
    if !dst.try_exists().map_err(BinaryError::InvalidDirectory)? {
        return Ok(false);
    }

    // Raise an error if it is a file
    if dst.is_file() {
        return Err(BinaryError::DirectoryIsFile(dst.display().to_string()));
    }

    // Check if the checksum is valid
    // If a checksum is not specified, assume the directory is invalid
    if let Some(ch) = checksum {
        let file = dst.join("checksum");
        Ok(file.is_file()
            && ch == fs::read_to_string(file).map_err(BinaryError::InvalidDirectory)?)
    } else {
        Ok(false)
    }
}

/// A lock whose owner shows no sign of life for this long is taken over: the owner was killed.
#[cfg(not(test))]
const LOCK_STALE: Duration = Duration::from_secs(30);
#[cfg(test)]
const LOCK_STALE: Duration = Duration::from_millis(800);

/// A failure recorded by a lock's owner counts for those that started waiting up to this long
/// before it, to allow for the file system's coarse timestamps.
#[cfg(not(test))]
const FAILURE_GRACE: Duration = Duration::from_secs(2);
#[cfg(test)]
const FAILURE_GRACE: Duration = Duration::from_millis(100);

/// Decides when the owner of a lock is gone.
struct Watch {
    seen: Option<(Vec<Option<SystemTime>>, Instant)>,
    last_check: Instant,
}

impl Watch {
    fn new(now: Instant) -> Self {
        Self {
            seen: None,
            last_check: now,
        }
    }

    fn reset(&mut self) {
        self.seen = None;
    }

    /// Whether a lock showing these signs of life, at `now`, looks abandoned: they have not
    /// changed for a whole window. The clock is ours (monotonic), so a wall clock that jumps or is
    /// skewed cannot make a live owner look dead.
    fn abandoned(&mut self, signs: Vec<Option<SystemTime>>, now: Instant) -> bool {
        // Time in which we did not run is no evidence: if the machine was suspended, or this
        // process stalled, the owner was most likely stopped as well and could not show a sign.
        // The clock may or may not have counted that time, so start counting again either way.
        if now.duration_since(self.last_check) > LOCK_STALE / 2 {
            self.seen = None;
        }
        self.last_check = now;
        match &self.seen {
            Some((last, since)) if *last == signs => now.duration_since(*since) > LOCK_STALE,
            _ => {
                self.seen = Some((signs, now));
                false
            }
        }
    }
}

/// Held while one process downloads and unpacks a bundle. Several build scripts can ask for the
/// same bundle at once, so the others wait for the checksum file instead of downloading it again.
///
/// The lock is a directory next to the bundle's: creating one is atomic on every platform. While
/// it is held, a thread keeps rewriting a heartbeat file inside it, so that a lock left behind by
/// a process that was killed (Ctrl-C during a long download) is noticed and taken over instead of
/// making every later build wait for an owner that is gone.
struct DirLock {
    path: PathBuf,
    failure: PathBuf,
    stop: Option<mpsc::Sender<()>>,
    beat: Option<thread::JoinHandle<()>>,
}

impl DirLock {
    fn path_of(dst: &Path) -> PathBuf {
        Self::sibling(dst, ".lock")
    }

    /// Where an owner that failed leaves its error for the others.
    fn failure_path_of(dst: &Path) -> PathBuf {
        Self::sibling(dst, ".failed")
    }

    fn sibling(dst: &Path, suffix: &str) -> PathBuf {
        let mut path = dst.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    }

    /// The error an owner left after we started waiting, if any.
    fn failure_since(dst: &Path, started: SystemTime) -> Option<BinaryError> {
        let failure = Self::failure_path_of(dst);
        let at = fs::metadata(&failure).and_then(|m| m.modified()).ok()?;
        if at < started {
            return None;
        }
        let error = fs::read_to_string(&failure).unwrap_or_default();
        Some(BinaryError::DownloadFailedElsewhere(error))
    }

    /// `acquire`, for the bundle at `dst`: it is done once it is valid.
    fn acquire_bundle(dst: &Path, checksum: Option<&str>) -> Result<Option<Self>, BinaryError> {
        Self::acquire(dst, || check_valid_dir(dst, checksum))
    }

    /// Takes the lock named after `dst`, or returns `None` once `done` says the work is finished
    /// while waiting. If the owner fails meanwhile, so do we, with its error: retrying one after
    /// the other would repeat a slow failure once per waiter.
    fn acquire(
        dst: &Path,
        done: impl Fn() -> Result<bool, BinaryError>,
    ) -> Result<Option<Self>, BinaryError> {
        let lock = Self::path_of(dst);
        let started = SystemTime::now() - FAILURE_GRACE;
        let mut watch = Watch::new(Instant::now());
        loop {
            match fs::create_dir(&lock) {
                Ok(()) => {
                    // Another process may have finished (or failed) between our check and the lock.
                    if done()? {
                        let _ = fs::remove_dir(&lock);
                        return Ok(None);
                    }
                    if let Some(error) = Self::failure_since(dst, started) {
                        let _ = fs::remove_dir(&lock);
                        return Err(error);
                    }
                    // An error left by an earlier build is not ours to report.
                    let _ = fs::remove_file(Self::failure_path_of(dst));
                    return Ok(Some(Self::start_heartbeat(
                        lock,
                        Self::failure_path_of(dst),
                    )));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if done()? {
                        return Ok(None);
                    }
                    if let Some(error) = Self::failure_since(dst, started) {
                        return Err(error);
                    }
                    let signs: Vec<Option<SystemTime>> = [lock.clone(), lock.join("heartbeat")]
                        .iter()
                        .map(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
                        .collect();
                    if watch.abandoned(signs, Instant::now()) {
                        // Nobody touched the lock for a whole window: its owner is gone.
                        let _ = fs::remove_dir_all(&lock);
                        watch.reset();
                        continue;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(BinaryError::InvalidDirectory(e)),
            }
        }
    }

    /// Tells the build scripts waiting for us that this failed, so that they fail too.
    fn record_failure(&self, error: &dyn std::fmt::Display) {
        let _ = fs::write(&self.failure, error.to_string());
    }

    fn start_heartbeat(path: PathBuf, failure: PathBuf) -> Self {
        let (stop, rx) = mpsc::channel::<()>();
        let heartbeat = path.join("heartbeat");
        let beat = thread::spawn(move || {
            let mut count = 0u64;
            loop {
                // The content changes every time, so the modification time always does.
                count += 1;
                let _ = fs::write(&heartbeat, count.to_string());
                if !matches!(
                    rx.recv_timeout(LOCK_STALE / 4),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
            }
        });
        Self {
            path,
            failure,
            stop: Some(stop),
            beat: Some(beat),
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(beat) = self.beat.take() {
            let _ = beat.join();
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// `make_available`, unless another process is already doing it or has done it.
fn make_available_once(bin: UrlBinary, dst: &Path) -> Result<(), BinaryError> {
    match DirLock::acquire_bundle(dst, bin.checksum.as_deref())? {
        Some(lock) => {
            let result = make_available(bin, dst);
            if let Err(error) = &result {
                lock.record_failure(error);
            }
            result
        }
        None => Ok(()),
    }
}

/// Retrieve a binary archive from the specified `url` and decompress it in the target directory.
/// "Download" is used as an umbrella term, since this can also be a local file.
fn make_available(bin: UrlBinary, dst: &Path) -> Result<(), BinaryError> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    // Check whether the file is local or not
    // file:// URLs use three slashes for absolute paths (file:///path),
    // leaving a leading '/' after stripping "file://". On Windows this
    // produces "/C:/..." which is not a valid path, so strip it.
    let (url, local) = match bin.url.strip_prefix("file://") {
        #[cfg(windows)]
        Some(file) => (file.strip_prefix('/').unwrap_or(file), true),
        #[cfg(not(windows))]
        Some(file) => (file, true),
        None => (bin.url.as_str(), false),
    };

    let ext = url.try_into()?;

    // Check if it is a folder and it can be linked
    if matches!(ext, Extension::Folder) {
        if !local {
            return Err(BinaryError::UnsupportedExtension("<folder>".into()));
        }
        let _l = LOCK.get_or_init(|| Mutex::new(())).lock();
        create_link(Path::new(url), dst)?;
        return Ok(());
    }

    // Use a local file or download from the web
    let (file, expected_len) = if local {
        (fs::read(url).map_err(BinaryError::LocalFileError)?, None)
    } else {
        let res = attohttpc::get(url)
            .header("User-Agent", "system-deps")
            .send()?;
        if res.status() == attohttpc::StatusCode::IM_A_TEAPOT {
            return Err(BinaryError::AntiBot(url.into()));
        }
        let res = res.error_for_status()?;
        let expected_len = expected_len(res.headers());
        (res.bytes()?, expected_len)
    };

    // Verify the checksum
    let calculated = sha256::digest(&*file);
    let checksum = match bin.checksum {
        Some(ch) if *ch == calculated => Ok(ch),
        _ => match expected_len {
            Some(len) if (file.len() as u64) < len => Err(BinaryError::DownloadError(
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("{url}: received {} of {len} bytes", file.len()),
                )
                .into(),
            )),
            _ => Err(BinaryError::InvalidChecksum(
                url.into(),
                bin.checksum.unwrap_or("<empty>".into()),
                calculated,
            )),
        },
    }?;
    fs::create_dir_all(dst).map_err(BinaryError::DecompressError)?;

    // Decompress the binary archive
    decompress(&file, dst, ext)?;

    // Generate info.toml with auto-detected pkgconfig paths
    create_info_file(dst)?;

    // Write the checksum last so that concurrent build scripts of other
    // crates (each running their own `system-deps` build.rs) cannot observe
    // a "valid" directory until extraction and info.toml are fully written.
    // Without this, `check_valid_dir` would return true mid-decompress and
    // `find_pkgconfig_dirs` would observe an incomplete tree, producing an
    // empty `paths.toml` entry (race condition).
    fs::write(dst.join("checksum"), checksum).map_err(BinaryError::DecompressError)?;

    Ok(())
}

/// The body length announced by the server, unless the body is compressed.
fn expected_len(headers: &attohttpc::header::HeaderMap) -> Option<u64> {
    if headers.contains_key(attohttpc::header::CONTENT_ENCODING) {
        return None;
    }
    headers
        .get(attohttpc::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// Generate an `info.toml` file listing all directories containing `.pc` files.
/// Skips generation if `info.toml` already exists.
fn create_info_file(dst: &Path) -> Result<(), BinaryError> {
    let info_path = dst.join("info.toml");
    if info_path.exists() {
        return Ok(());
    }

    let pc_dirs: Vec<String> = find_pkgconfig_dirs(dst)
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(dst)
                .ok()
                .map(|rel| rel.to_string_lossy().into_owned())
        })
        .collect();

    let mut table = toml::Table::new();
    table.insert(
        "paths".to_string(),
        toml::Value::Array(pc_dirs.into_iter().map(toml::Value::String).collect()),
    );

    fs::write(
        info_path,
        toml::to_string(&table)
            .map_err(|e| BinaryError::DecompressError(std::io::Error::other(e)))?,
    )
    .map_err(BinaryError::DecompressError)
}

/// Extract a binary archive to the target directory. The methods for unpacking are
/// different depending on the extension. Each file type is gated behind a feature to
/// avoid having too many dependencies.
fn decompress(_file: &[u8], _dst: &Path, ext: Extension) -> Result<(), BinaryError> {
    match ext {
        #[cfg(feature = "gz")]
        Extension::TarGz => {
            let reader = flate2::read::GzDecoder::new(_file);
            let mut archive = tar::Archive::new(reader);
            archive.unpack(_dst).map_err(BinaryError::DecompressError)
        }
        #[cfg(feature = "xz")]
        Extension::TarXz => {
            // `true`: allow concatenated xz streams, matching the
            // multi-stream tolerance of the previous liblzma decoder.
            let reader = lzma_rust2::XzReader::new(_file, true);
            let mut archive = tar::Archive::new(reader);
            archive.unpack(_dst).map_err(BinaryError::DecompressError)
        }
        #[cfg(feature = "zip")]
        Extension::Zip => {
            let reader = std::io::Cursor::new(_file);
            let mut archive =
                zip::ZipArchive::new(reader).map_err(|e| BinaryError::DecompressError(e.into()))?;
            archive
                .extract(_dst)
                .map_err(|e| BinaryError::DecompressError(e.into()))
        }
        #[cfg(all(feature = "pkg", target_os = "macos"))]
        Extension::Pkg => {
            let reader = std::io::Cursor::new(_file);
            pkg_extractor::PkgExtractor::new(reader, Some(_dst.into()))
                .extract()
                .map_err(|e| BinaryError::DecompressError(std::io::Error::other(format!("{e:?}"))))
        }
        #[cfg(all(feature = "inno", target_os = "windows"))]
        Extension::Inno => {
            let cursor = std::io::Cursor::new(_file);
            let mut inno = inno::Inno::new(cursor)
                .map_err(|e| BinaryError::DecompressError(std::io::Error::other(e.to_string())))?;
            for file in inno.files() {
                let (entry, data) = file.map_err(|e| {
                    BinaryError::DecompressError(std::io::Error::other(e.to_string()))
                })?;
                let Some(destination) = entry.file().normalized_destination() else {
                    continue;
                };
                if destination.is_empty() {
                    continue;
                }
                let path = _dst.join(destination);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(BinaryError::DecompressError)?;
                }
                fs::write(&path, &data).map_err(BinaryError::DecompressError)?;
            }
            Ok(())
        }
        _ => unreachable!(),
    }
}

/// Create a symlink (Unix) or junction (Windows) from `dst` pointing to `src`.
/// If `dst` already points to `src`, this is a no-op.
fn create_link(src: &Path, dst: &Path) -> Result<(), BinaryError> {
    if dst.read_link().is_ok_and(|l| l == src) {
        return Ok(());
    }
    if dst.read_link().is_ok() {
        // Remove existing symlink/junction
        #[cfg(unix)]
        fs::remove_file(dst).map_err(BinaryError::SymlinkError)?;
        #[cfg(windows)]
        fs::remove_dir(dst).map_err(BinaryError::SymlinkError)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(src, dst).map_err(BinaryError::SymlinkError)?;
    // Use a junction on Windows instead of a symlink to avoid requiring
    // administrator privileges or Developer Mode.
    #[cfg(windows)]
    junction::create(src, dst).map_err(BinaryError::SymlinkError)?;
    Ok(())
}

pub fn merge(rhs: &mut Table, lhs: Table, force: bool) -> Result<(), Error> {
    // Update the values for url and follows
    if force {
        for (key, value) in lhs.iter() {
            if value.get("url").is_some() {
                if let Some(Value::Table(pkg)) = rhs.get_mut(key) {
                    pkg.remove("follows");
                }
            }
            if let Some(Value::Array(provides)) = value.get("provides") {
                for name in provides {
                    let name = name.as_str().ok_or(Error::IncompatibleMerge)?;
                    let pkg = rhs
                        .entry(name)
                        .or_insert(Value::Table(Table::new()))
                        .as_table_mut()
                        .unwrap();
                    pkg.insert("follows".into(), Value::String(key.into()));
                    pkg.remove("url");
                }
            }
        }
    }

    // The regular merge
    crate::parse::merge(rhs, lhs, force)?;

    // Don't allow both url and follows for the same package
    for value in rhs.values() {
        if value.get("url").is_some() && value.get("follows").is_some() {
            return Err(Error::IncompatibleMerge);
        }
    }

    Ok(())
}

/// Name of the variable that disables the prebuilt binaries of `lib`, or all of them.
pub fn no_prebuilt_var(lib: Option<&str>) -> String {
    match lib {
        Some(lib) => format!("SYSTEM_DEPS_{}_NO_PREBUILT", lib.to_shouty_snake_case()),
        None => "SYSTEM_DEPS_NO_PREBUILT".into(),
    }
}

/// Removes the binaries disabled with `SYSTEM_DEPS_$NAME_NO_PREBUILT`, and those following them.
///
/// `is_set` tells whether a variable is set, and is called once for every binary.
pub fn skip_disabled(binaries: Table, mut is_set: impl FnMut(&str) -> bool) -> Table {
    let skipped: Vec<String> = binaries
        .keys()
        .filter(|name| is_set(&no_prebuilt_var(Some(name))))
        .cloned()
        .collect();
    binaries
        .into_iter()
        .filter(|(name, value)| {
            let follows = value.get("follows").and_then(|f| f.as_str());
            !skipped.contains(name) && !follows.is_some_and(|f| skipped.iter().any(|s| s == f))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("system_deps_test_{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn find_pkgconfig_empty_dir() {
        let dir = create_temp_dir("empty");
        let result = find_pkgconfig_dirs(&dir);
        assert!(result.is_empty());
    }

    #[test]
    fn find_pkgconfig_nonexistent_dir() {
        let dir = PathBuf::from("/tmp/system_deps_test_nonexistent_dir_that_does_not_exist");
        let result = find_pkgconfig_dirs(&dir);
        assert!(result.is_empty());
    }

    #[test]
    fn find_pkgconfig_single() {
        let dir = create_temp_dir("single");
        fs::create_dir_all(dir.join("lib/pkgconfig")).unwrap();
        let result = find_pkgconfig_dirs(&dir);
        assert_eq!(result, vec![dir.join("lib/pkgconfig")]);
    }

    #[test]
    fn find_pkgconfig_multiple() {
        let dir = create_temp_dir("multiple");
        fs::create_dir_all(dir.join("lib/pkgconfig")).unwrap();
        fs::create_dir_all(dir.join("share/pkgconfig")).unwrap();

        let mut result = find_pkgconfig_dirs(&dir);
        result.sort();
        let mut expected = vec![dir.join("lib/pkgconfig"), dir.join("share/pkgconfig")];
        expected.sort();
        assert_eq!(result, expected);
    }

    #[test]
    fn find_pkgconfig_nested_depths() {
        let dir = create_temp_dir("nested");
        fs::create_dir_all(dir.join("a/b/pkgconfig")).unwrap();
        fs::create_dir_all(dir.join("c/pkgconfig")).unwrap();

        let mut result = find_pkgconfig_dirs(&dir);
        result.sort();
        let mut expected = vec![dir.join("a/b/pkgconfig"), dir.join("c/pkgconfig")];
        expected.sort();
        assert_eq!(result, expected);
    }

    #[test]
    fn find_pkgconfig_file_ignored() {
        let dir = create_temp_dir("file_ignored");
        // Create a file named "pkgconfig" — should be ignored
        fs::write(dir.join("pkgconfig"), "not a directory").unwrap();
        // Create a real pkgconfig dir elsewhere
        fs::create_dir_all(dir.join("lib/pkgconfig")).unwrap();

        let result = find_pkgconfig_dirs(&dir);
        assert_eq!(result, vec![dir.join("lib/pkgconfig")]);
    }

    /// Serve a single response with the given `headers` and `body`.
    #[cfg(feature = "xz")]
    fn serve_once(headers: &str, body: &[u8]) -> String {
        use std::io::{Read, Write};

        let response = [
            format!("HTTP/1.1 200 OK\r\n{headers}Connection: close\r\n\r\n").as_bytes(),
            body,
        ]
        .concat();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            stream.write_all(&response).unwrap();
        });
        format!("http://127.0.0.1:{port}/archive.tar.xz")
    }

    #[cfg(feature = "xz")]
    fn download(url: String, name: &str) -> Result<(), BinaryError> {
        let bin = UrlBinary {
            url,
            checksum: Some("0".repeat(64)),
            paths: None,
        };
        make_available(bin, &create_temp_dir(name).join("dst"))
    }

    #[cfg(feature = "xz")]
    #[test]
    fn truncated_download() {
        let url = serve_once("Content-Length: 100\r\n", &[0; 10]);
        let err = download(url, "truncated").unwrap_err();
        assert!(matches!(err, BinaryError::DownloadError(_)), "{}", err);
        assert!(
            err.to_string().contains("received 10 of 100 bytes"),
            "{}",
            err
        );
    }

    #[cfg(feature = "xz")]
    #[test]
    fn complete_download_with_wrong_checksum() {
        let url = serve_once("Content-Length: 10\r\n", &[0; 10]);
        let err = download(url, "complete").unwrap_err();
        assert!(matches!(err, BinaryError::InvalidChecksum(..)), "{}", err);
    }

    /// The announced length is that of the compressed body, not of the received one.
    #[cfg(feature = "xz")]
    #[test]
    fn compressed_download_with_wrong_checksum() {
        // Ten zero bytes, gzip compressed.
        const GZIP: [u8; 23] = [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0x63, 0x60, 0x80, 0x01,
            0x00, 0x76, 0x68, 0x8a, 0xe3, 0x0a, 0x00, 0x00, 0x00,
        ];
        let url = serve_once("Content-Encoding: gzip\r\nContent-Length: 23\r\n", &GZIP);
        let err = download(url, "compressed").unwrap_err();
        assert!(matches!(err, BinaryError::InvalidChecksum(..)), "{}", err);
    }

    /// Build scripts of several crates ask for the same bundle at once: only one downloads it.
    #[test]
    fn only_one_thread_downloads_a_bundle() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let dst = create_temp_dir("lock").join("bundle");
        let checksum = "a".repeat(64);
        let owners = AtomicUsize::new(0);

        thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    if let Some(_lock) = DirLock::acquire_bundle(&dst, Some(&checksum)).unwrap() {
                        owners.fetch_add(1, AtomicOrdering::SeqCst);
                        thread::sleep(Duration::from_millis(200));
                        fs::create_dir_all(&dst).unwrap();
                        fs::write(dst.join("checksum"), &checksum).unwrap();
                    }
                });
            }
        });

        assert_eq!(owners.load(AtomicOrdering::SeqCst), 1);
        assert!(check_valid_dir(&dst, Some(&checksum)).unwrap());
        // The lock is released at the end
        assert!(!dst.with_extension("lock").exists());
    }

    #[test]
    fn no_prebuilt_var_names() {
        assert_eq!(no_prebuilt_var(None), "SYSTEM_DEPS_NO_PREBUILT");
        assert_eq!(
            no_prebuilt_var(Some("gstreamer_1_0")),
            "SYSTEM_DEPS_GSTREAMER_1_0_NO_PREBUILT"
        );
    }

    #[test]
    fn skip_disabled_binaries() {
        let binaries: Table = toml::from_str(
            r#"
            liba = { url = "file:///a" }
            libb = { follows = "liba" }
            libc = { url = "file:///c" }
            libd = { follows = "libc" }
            "#,
        )
        .unwrap();
        let skip = |set: &[&str]| {
            let mut checked = Vec::new();
            let kept: Vec<String> = skip_disabled(binaries.clone(), |var| {
                checked.push(var.to_string());
                set.contains(&var)
            })
            .into_iter()
            .map(|(name, _)| name)
            .collect();
            assert_eq!(checked.len(), 4, "{:?}", checked);
            kept
        };

        assert_eq!(skip(&[]), ["liba", "libb", "libc", "libd"]);
        assert_eq!(skip(&["SYSTEM_DEPS_LIBA_NO_PREBUILT"]), ["libc", "libd"]);
        assert_eq!(
            skip(&["SYSTEM_DEPS_LIBB_NO_PREBUILT"]),
            ["liba", "libc", "libd"]
        );
        assert_eq!(
            skip(&[
                "SYSTEM_DEPS_LIBA_NO_PREBUILT",
                "SYSTEM_DEPS_LIBD_NO_PREBUILT"
            ]),
            ["libc"]
        );
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bundle(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("system_deps_test_{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("bundle")
    }

    /// A lock directory left behind by a process that was killed does not block the next build.
    #[test]
    fn abandoned_lock_is_taken_over() {
        let dst = bundle("abandoned");
        fs::create_dir(DirLock::path_of(&dst)).unwrap();

        let start = Instant::now();
        let lock = DirLock::acquire_bundle(&dst, Some(&"a".repeat(64))).unwrap();
        assert!(lock.is_some());
        assert!(start.elapsed() >= LOCK_STALE, "{:?}", start.elapsed());
        assert!(start.elapsed() < LOCK_STALE * 5, "{:?}", start.elapsed());
    }

    /// A live owner that takes much longer than the staleness window keeps its lock.
    #[test]
    fn live_owner_keeps_the_lock_while_it_works() {
        let dst = bundle("live");
        let checksum = "a".repeat(64);
        let owners = AtomicUsize::new(0);

        thread::scope(|s| {
            s.spawn(|| {
                let _lock = DirLock::acquire_bundle(&dst, Some(&checksum))
                    .unwrap()
                    .unwrap();
                owners.fetch_add(1, Ordering::SeqCst);
                thread::sleep(LOCK_STALE * 4);
                fs::create_dir_all(&dst).unwrap();
                fs::write(dst.join("checksum"), &checksum).unwrap();
            });
            thread::sleep(LOCK_STALE / 2);
            for _ in 0..3 {
                s.spawn(|| {
                    if DirLock::acquire_bundle(&dst, Some(&checksum))
                        .unwrap()
                        .is_some()
                    {
                        owners.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });

        assert_eq!(owners.load(Ordering::SeqCst), 1);
        assert!(!DirLock::path_of(&dst).exists());
    }

    /// When the owner fails, the others fail with its error instead of trying again in turn.
    #[test]
    fn waiters_fail_with_the_owners_error() {
        let dst = bundle("failed");
        let checksum = "a".repeat(64);
        let owners = AtomicUsize::new(0);
        let failures = AtomicUsize::new(0);

        thread::scope(|s| {
            s.spawn(|| {
                let lock = DirLock::acquire_bundle(&dst, Some(&checksum))
                    .unwrap()
                    .unwrap();
                owners.fetch_add(1, Ordering::SeqCst);
                thread::sleep(LOCK_STALE / 2);
                lock.record_failure(&BinaryError::AntiBot("https://example/archive".into()));
            });
            thread::sleep(Duration::from_millis(100));
            for _ in 0..4 {
                s.spawn(|| match DirLock::acquire_bundle(&dst, Some(&checksum)) {
                    Err(BinaryError::DownloadFailedElsewhere(error)) => {
                        assert!(error.contains("https://example/archive"), "{}", error);
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                    other => panic!("expected the owner's failure, got {:?}", other.map(|_| ())),
                });
            }
        });

        assert_eq!(owners.load(Ordering::SeqCst), 1);
        assert_eq!(failures.load(Ordering::SeqCst), 4);
    }

    /// An error left by an earlier build does not stop a later one from trying.
    #[test]
    fn an_earlier_failure_does_not_block_a_new_build() {
        let dst = bundle("earlier");
        fs::write(DirLock::failure_path_of(&dst), "an old error").unwrap();
        thread::sleep(FAILURE_GRACE * 3);

        let lock = DirLock::acquire_bundle(&dst, Some(&"a".repeat(64))).unwrap();
        assert!(lock.is_some());
        assert!(!DirLock::failure_path_of(&dst).exists());
    }

    fn signs(secs: u64) -> Vec<Option<SystemTime>> {
        vec![
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            None,
        ]
    }

    #[test]
    fn watch_declares_a_quiet_lock_abandoned() {
        let t0 = Instant::now();
        let mut watch = Watch::new(t0);
        let step = LOCK_STALE / 4;
        assert!(!watch.abandoned(signs(1), t0));
        assert!(!watch.abandoned(signs(1), t0 + step));
        assert!(!watch.abandoned(signs(1), t0 + step * 2));
        assert!(!watch.abandoned(signs(1), t0 + step * 3));
        assert!(watch.abandoned(signs(1), t0 + LOCK_STALE + step / 2));
    }

    #[test]
    fn watch_keeps_a_lock_that_shows_signs_of_life() {
        let t0 = Instant::now();
        let mut watch = Watch::new(t0);
        let step = LOCK_STALE / 4;
        for i in 0..40u32 {
            // The owner's heartbeat changes the signs every step.
            assert!(!watch.abandoned(signs(i as u64), t0 + step * i));
        }
    }

    /// The machine was suspended: the owner could not beat either, so the window starts over.
    #[test]
    fn watch_does_not_count_time_we_did_not_run() {
        let t0 = Instant::now();
        let mut watch = Watch::new(t0);
        assert!(!watch.abandoned(signs(1), t0));

        // A long gap between two checks (whatever the clock says about it) and still nothing new
        let resumed = t0 + LOCK_STALE * 10;
        assert!(!watch.abandoned(signs(1), resumed));

        // Only a whole quiet window after the resume, checked as often as the real loop does,
        // makes the lock abandoned.
        let step = LOCK_STALE / 8;
        let declared = (1..=12u32)
            .map(|i| step * i)
            .find(|elapsed| watch.abandoned(signs(1), resumed + *elapsed))
            .expect("a quiet lock is abandoned in the end");
        assert!(declared > LOCK_STALE, "{:?}", declared);
        assert!(declared <= LOCK_STALE + step, "{:?}", declared);
    }

    fn unique_key(name: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("test-{}-{}-{}", name, std::process::id(), nanos)
    }

    fn some_paths(dir: &Path) -> CachedPaths {
        let mut paths = Paths::default();
        paths.paths.insert("lib".into(), vec![dir.to_path_buf()]);
        CachedPaths {
            vars: vec!["SYSTEM_DEPS_LIB_NO_PREBUILT".into()],
            paths,
        }
    }

    /// Many build scripts need the paths at once: one works them out, the others read them.
    #[test]
    fn the_paths_are_worked_out_once() {
        let key = unique_key("once");
        let dir = bundle("cache_once");
        let runs = AtomicUsize::new(0);

        thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let cached = Paths::cached(&key, || {
                        runs.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(200));
                        Ok(some_paths(dir.parent().unwrap()))
                    })
                    .unwrap();
                    // Everyone learns which variables to tell cargo about, not only the first
                    assert_eq!(cached.vars, ["SYSTEM_DEPS_LIB_NO_PREBUILT"]);
                });
            }
        });
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // A later build script, or build, just reads the file
        let again = Paths::cached(&key, || panic!("the paths should have been read")).unwrap();
        assert_eq!(again.vars, ["SYSTEM_DEPS_LIB_NO_PREBUILT"]);
    }

    /// If working the paths out fails, the build scripts waiting for them fail with that error.
    #[test]
    fn a_failure_is_shared_with_the_waiters() {
        let key = unique_key("shared_failure");
        let failures = AtomicUsize::new(0);

        thread::scope(|s| {
            s.spawn(|| {
                let result = Paths::cached(&key, || {
                    thread::sleep(LOCK_STALE / 2);
                    Err(BinaryError::AntiBot("https://example/archive".into()).into())
                });
                assert!(result.is_err());
            });
            thread::sleep(Duration::from_millis(100));
            for _ in 0..3 {
                s.spawn(
                    || match Paths::cached(&key, || panic!("must not be worked out again")) {
                        Err(Error::Binary(BinaryError::DownloadFailedElsewhere(message))) => {
                            assert!(message.contains("https://example/archive"), "{}", message);
                            failures.fetch_add(1, Ordering::SeqCst);
                        }
                        other => {
                            panic!("expected the failure of the first, got {:?}", other.is_ok())
                        }
                    },
                );
            }
        });
        assert_eq!(failures.load(Ordering::SeqCst), 3);
    }

    /// A copy that lists directories that are gone (a cleaned target directory) is not trusted.
    #[test]
    fn paths_whose_directories_are_gone_are_worked_out_again() {
        let key = unique_key("gone");
        let first = bundle("cache_gone_first");
        let second = bundle("cache_gone_second");
        fs::create_dir_all(&first).unwrap();
        Paths::cached(&key, || Ok(some_paths(&first))).unwrap();

        fs::remove_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let runs = AtomicUsize::new(0);
        let cached = Paths::cached(&key, || {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(some_paths(&second))
        })
        .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(cached.paths.paths["lib"], [second]);
    }

    #[test]
    fn the_key_depends_on_every_input_and_where_it_ends() {
        assert_eq!(cache_key(&[b"a", b"b"]), cache_key(&[b"a", b"b"]));
        assert_ne!(cache_key(&[b"a", b"b"]), cache_key(&[b"a", b"c"]));
        // Moving a byte from one input to the next is another key
        assert_ne!(cache_key(&[b"ab", b"c"]), cache_key(&[b"a", b"bc"]));
    }
}

/// The lock between real processes: each one is a copy of the test executable, running `child`.
#[cfg(test)]
mod process_tests {
    use super::*;
    use std::{
        io::Write,
        process::{Child, Command, Stdio},
    };

    const CHECKSUM: &str = "b";

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("system_deps_process_{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spawn(dir: &Path, role: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["process_tests::child", "--nocapture", "--test-threads=1"])
            .env("SYSTEM_DEPS_LOCK_TEST_DIR", dir)
            .env("SYSTEM_DEPS_LOCK_TEST_ROLE", role)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Not a test by itself: the body of the processes the tests below start.
    #[test]
    fn child() {
        let (Some(dir), Some(role)) = (
            std::env::var_os("SYSTEM_DEPS_LOCK_TEST_DIR"),
            std::env::var("SYSTEM_DEPS_LOCK_TEST_ROLE").ok(),
        ) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let dst = dir.join("bundle");
        let Some(_lock) = DirLock::acquire_bundle(&dst, Some(CHECKSUM)).unwrap() else {
            return;
        };
        let mut owners = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("owners.log"))
            .unwrap();
        writeln!(owners, "{}", std::process::id()).unwrap();
        drop(owners);
        if role == "hang" {
            fs::write(dir.join("hanging"), "").unwrap();
            thread::sleep(Duration::from_secs(120));
        }
        thread::sleep(LOCK_STALE * 2);
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("checksum"), CHECKSUM).unwrap();
    }

    fn owners(dir: &Path) -> usize {
        fs::read_to_string(dir.join("owners.log"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Several processes want the same bundle: one makes it, the others wait for it.
    #[test]
    fn only_one_process_makes_a_bundle() {
        let dir = dir("one");
        let children: Vec<Child> = (0..6).map(|_| spawn(&dir, "worker")).collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(owners(&dir), 1);
        assert!(dir.join("bundle").join("checksum").is_file());
        assert!(!DirLock::path_of(&dir.join("bundle")).exists());
    }

    /// The owner is killed in the middle of the work: the next process takes over.
    #[test]
    fn a_killed_process_does_not_block_the_next() {
        let dir = dir("killed");
        let mut owner = spawn(&dir, "hang");
        let start = Instant::now();
        while !dir.join("hanging").exists() {
            assert!(start.elapsed() < Duration::from_secs(20), "no owner");
            thread::sleep(Duration::from_millis(20));
        }
        owner.kill().unwrap();
        owner.wait().unwrap();
        assert!(DirLock::path_of(&dir.join("bundle")).exists());

        let start = Instant::now();
        assert!(spawn(&dir, "worker").wait().unwrap().success());
        assert!(start.elapsed() < LOCK_STALE * 10, "{:?}", start.elapsed());
        assert_eq!(owners(&dir), 2);
        assert!(dir.join("bundle").join("checksum").is_file());
    }
}
