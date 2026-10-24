//! Tacita's folder and private-file helpers.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `$TACITA_HOME` if set, else `~/Library/Application Support/Tacita`.
pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("TACITA_HOME") {
        return PathBuf::from(h);
    }
    match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join("Library/Application Support/Tacita"),
        None => {
            eprintln!("Tacita: neither TACITA_HOME nor HOME is set");
            std::process::exit(1);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
