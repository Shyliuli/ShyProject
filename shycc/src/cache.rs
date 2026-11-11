//! Memoizes the artifacts shycc builds over and over for the same inputs.
//!
//! Two kinds of work repeat on every link: compiling the toolchain's own
//! libraries (`libshy` and the soft-float runtime) from sources the user did not
//! touch, and evaluating the `const fn` calls those libraries and the user's
//! program contain. Both are pure functions of their inputs, so both are cached
//! on disk here.
//!
//! What a key covers is the whole point: an artifact is reused only when every
//! input that can change it is in the key. For a library unit that is the
//! compiler binary, the target, the code-generation flags, and the bytes of the
//! entire library tree (headers included). For a probe answer set it is all of
//! that plus the user's own source and include directories.
//!
//! The root is `SHYCC_CACHE_DIR`, or `$TMPDIR/shycc-cache`; nothing is written
//! outside it. `SHYCC_NO_CACHE=1` and `--no-cache` turn the cache off, and a
//! cache that cannot be read or written is not a build error: the caller builds
//! the artifact as it always did and the result is still correct.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The key layout. Bump it whenever what a key covers changes, so an old
/// directory is never read with a new meaning.
const KEY_VERSION: &str = "shycc-cache-1";

/// The largest tree the cache will fingerprint.
///
/// Reading every file of a big include directory would cost more than the
/// compile it saves, so past this limit the caller does not use the cache at
/// all. Failing towards "compile it" is always correct.
pub const MAX_TREE_FILES: usize = 4096;

static CACHE: OnceLock<Cache> = OnceLock::new();

/// The cache this process uses.
///
/// [`Cache::install`] must run before the first compile step; a driver that
/// never installs one gets the disabled cache, which builds everything.
pub fn cache() -> &'static Cache {
    CACHE.get_or_init(|| Cache { root: None })
}

/// A 128-bit FNV-1a digest of the inputs that decide one artifact.
///
/// A cache key has to be order-sensitive and unambiguous about field
/// boundaries, so every field is followed by a separator byte and every number
/// is written in a fixed width.
pub struct Fingerprint(u128);

/// The FNV-1a 128-bit parameters.
const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

/// The byte that closes a variable-length field, so `"ab" + "c"` and
/// `"a" + "bc"` cannot produce the same digest.
const SEPARATOR: u8 = 0xff;

impl Fingerprint {
    pub fn new() -> Self {
        Fingerprint(FNV_OFFSET)
    }

    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        for byte in bytes {
            self.0 ^= u128::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
        self
    }

    /// One variable-length field.
    pub fn text(&mut self, text: &str) -> &mut Self {
        self.bytes(text.as_bytes());
        self.bytes(&[SEPARATOR])
    }

    /// One fixed-width number.
    pub fn number(&mut self, value: u64) -> &mut Self {
        self.bytes(&value.to_le_bytes())
    }

    /// The key this fingerprint names.
    pub fn key(&self) -> String {
        format!("{:032x}", self.0)
    }
}

impl Default for Fingerprint {
    fn default() -> Self {
        Self::new()
    }
}

/// One directory of artifacts, or none when caching is off.
pub struct Cache {
    /// The versioned directory holding every artifact, or `None` when disabled.
    root: Option<PathBuf>,
}

impl Cache {
    /// Read the cache settings from `--no-cache` and the environment.
    pub fn from_env(disabled_by_flag: bool) -> Cache {
        if disabled_by_flag || env_disabled() {
            return Cache { root: None };
        }
        let root = match env::var_os("SHYCC_CACHE_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => env::temp_dir().join("shycc-cache"),
        };
        Cache {
            root: Some(root.join(KEY_VERSION)),
        }
    }

    /// Make the cache this process uses. Later calls are ignored, which is what
    /// a driver that installs it once at startup wants.
    pub fn install(cache: Cache) {
        let _ = CACHE.set(cache);
    }

    /// The directory holding the artifacts, when caching is on.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The stored artifact for `key`, when there is one.
    ///
    /// A zero-length file is a leftover from a build that was killed between
    /// its `rename` and its write, and it is never used.
    pub fn lookup(&self, key: &str, extension: &str) -> Option<PathBuf> {
        let path = self.path(key, extension)?;
        let usable = path
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.len() > 0);
        usable.then_some(path)
    }

    /// Store `artifact` under `key` and return the path later reads should use.
    ///
    /// The move is atomic, so a concurrent build sees either the whole file or
    /// no file, never a half-written one. A cache that cannot be written leaves
    /// the artifact where it is: caching is an optimization, and failing to
    /// cache must not fail the build.
    pub fn store(&self, key: &str, extension: &str, artifact: &Path) -> PathBuf {
        let Some(stored) = self.path(key, extension) else {
            return artifact.to_path_buf();
        };
        // `save_temps` state paths and cache paths never coincide, but a caller
        // that already stored the artifact has nothing left to do.
        if stored == artifact {
            return stored;
        }
        let Some(dir) = stored.parent() else {
            return artifact.to_path_buf();
        };
        if fs::create_dir_all(dir).is_err() {
            return artifact.to_path_buf();
        }
        if fs::rename(artifact, &stored).is_ok() {
            return stored;
        }
        // A cache directory on another filesystem cannot take a rename.
        if fs::copy(artifact, &stored).is_ok() {
            return stored;
        }
        artifact.to_path_buf()
    }

    fn path(&self, key: &str, extension: &str) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        Some(root.join(format!("{key}.{extension}")))
    }
}

/// Whether the environment turns the cache off.
///
/// Any non-empty value does; an empty one is what a shell leaves behind for
/// `SHYCC_NO_CACHE=`, which does not mean "yes".
fn env_disabled() -> bool {
    env::var_os("SHYCC_NO_CACHE").is_some_and(|value| !value.is_empty())
}

/// Add `path` and its bytes to `fingerprint`.
pub fn hash_file(fingerprint: &mut Fingerprint, path: &Path) -> io::Result<()> {
    let bytes = fs::read(path)?;
    fingerprint.text(&path.to_string_lossy());
    fingerprint.number(bytes.len() as u64);
    fingerprint.bytes(&bytes);
    Ok(())
}

/// Add every file under `root`, in path order, to `fingerprint`.
///
/// A tree past [`MAX_TREE_FILES`] is an error rather than a partial digest: a
/// key that misses a file would reuse an artifact that file should have
/// changed, and a wrong answer is much worse than a slow build.
pub fn hash_tree(fingerprint: &mut Fingerprint, root: &Path) -> io::Result<()> {
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    files.sort();
    if files.len() > MAX_TREE_FILES {
        return Err(io::Error::other(format!(
            "{} holds {} files, over the {MAX_TREE_FILES} file limit for caching",
            root.display(),
            files.len()
        )));
    }
    fingerprint.text(&root.to_string_lossy());
    for path in files {
        hash_file(fingerprint, &path)?;
    }
    Ok(())
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&entry.path(), out)?;
        } else if file_type.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}
