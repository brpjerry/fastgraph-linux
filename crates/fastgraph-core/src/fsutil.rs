//! Small filesystem helpers shared by the store and preferences.

use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `bytes` to a new temporary file beside `path`, flush it to disk and
/// rename it over `path`, so `path` holds either the old or the new contents
/// after a crash. The temporary file is created exclusively, so nothing else
/// is truncated or followed, and it is removed if anything fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = temp_for(path)?;
    tmp.write_all(bytes)
        .and_then(|()| tmp.as_file().sync_all())
        .with_context(|| format!("writing {}", path.display()))?;
    persist(tmp, path)
}

/// A new, exclusively created temporary file in `path`'s directory. Its name
/// starts with `.tmp`, so the store skips leftovers. It is created 0644 (less
/// the umask) like an ordinary file, not tempfile's private 0600, because
/// the rename keeps that mode on the exported or stored file.
pub(crate) fn temp_for(path: &Path) -> Result<tempfile::NamedTempFile> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o644))
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))
}

/// Renames a synced temporary file over `path`.
pub(crate) fn persist(tmp: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    tmp.persist(path)
        .map(drop)
        .map_err(|e| e.error)
        .with_context(|| format!("replacing {}", path.display()))
}

/// `xdg` if set to an absolute path, else `home/<fallback>`.
pub(crate) fn resolve_dir(
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
    fallback: &str,
) -> Result<PathBuf> {
    if let Some(x) = xdg {
        let p = Path::new(x);
        if p.is_absolute() {
            return Ok(p.to_path_buf());
        }
    }
    match home {
        Some(h) if !h.is_empty() => Ok(Path::new(h).join(fallback)),
        _ => {
            anyhow::bail!("cannot locate a data directory: neither XDG variable nor HOME is usable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn write_atomic_leaves_an_existing_tmp_sibling_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let sibling = dir.path().join("out.txt.tmp");
        std::fs::write(&sibling, b"keep me").unwrap();
        write_atomic(&path, b"new").unwrap();
        write_atomic(&path, b"newer").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"newer");
        assert_eq!(std::fs::read(&sibling).unwrap(), b"keep me");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    // Exports land where other tools and users read them: not private (0600).
    // Bugs caught: 0, False positives: 0
    #[test]
    fn written_files_are_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.txt");
        write_atomic(&path, b"x").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o044, 0o044, "mode {mode:o}");
    }
}
