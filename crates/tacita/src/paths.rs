//! Tacita's folder and private-file helpers.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `$TACITA_HOME` if set and non-empty, else `~/Library/Application Support/Tacita`.
pub fn home() -> PathBuf {
    match home_from(std::env::var_os("TACITA_HOME"), std::env::var_os("HOME")) {
        Some(h) => h,
        None => {
            eprintln!("Tacita: neither TACITA_HOME nor HOME is set");
            std::process::exit(1);
        }
    }
}

fn home_from(tacita_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let non_empty = |v: Option<OsString>| v.filter(|v| !v.is_empty());
    if let Some(h) = non_empty(tacita_home) {
        return Some(PathBuf::from(h));
    }
    non_empty(home).map(|h| PathBuf::from(h).join("Library/Application Support/Tacita"))
}

/// Creates the directory (and missing parents) and sets the leaf to 0700.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Writes a 0600 temp file in the same directory, then renames it into place.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Like `write_private`, but fails with `AlreadyExists` instead of replacing `path`.
pub fn create_private_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.new", std::process::id()));
    let result = write_private(&tmp, bytes).and_then(|()| fs::hard_link(&tmp, path));
    let _ = fs::remove_file(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tacita_home_counts_as_unset() {
        use std::ffi::OsString;
        let h = |t: &str, h: &str| home_from(Some(OsString::from(t)), Some(OsString::from(h)));
        assert_eq!(
            h("", "/Users/u"),
            Some(PathBuf::from("/Users/u/Library/Application Support/Tacita"))
        );
        assert_eq!(h("/x", "/Users/u"), Some(PathBuf::from("/x")));
        assert_eq!(home_from(None, Some(OsString::new())), None);
        assert_eq!(home_from(None, None), None);
    }

    #[test]
    fn create_private_new_refuses_existing() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("f");
        create_private_new(&p, b"one").unwrap();
        let e = create_private_new(&p, b"two").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&p).unwrap(), b"one");
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
    }

    #[test]
    fn dir_is_0700_even_if_it_existed() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("a/b");
        fs::create_dir_all(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn write_private_is_0600_and_replaces_without_leftovers() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("f");
        write_private(&p, b"one").unwrap();
        write_private(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
    }

    #[test]
    fn write_private_cleans_up_on_failure() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub"); // rename file onto a non-empty dir fails
        fs::create_dir(&p).unwrap();
        fs::write(p.join("x"), b"x").unwrap();
        assert!(write_private(&p, b"data").is_err());
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
    }
}
