//! Complete-file replacement for persistent configuration and credentials.

use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result};

/// Stage and sync a complete file before replacing its previous contents.
/// Errors preserve the previous destination and clean up the staged file.
pub fn write(path: &Path, payload: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed creating directory {}", parent.display()))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed staging file in {}", parent.display()))?;
    staged
        .write_all(payload)
        .with_context(|| format!("failed staging contents for {}", path.display()))?;
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("failed syncing staged contents for {}", path.display()))?;
    replace_staged(staged, path)
}

fn replace_staged(mut staged: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    let mut attempt = 0;
    loop {
        match staged.persist(path) {
            Ok(_) => return Ok(()),
            Err(error) => {
                // Readers and scanners can temporarily deny delete access on
                // Windows. Retry the same complete staged file for at most 150 ms.
                if cfg!(windows)
                    && attempt < 4
                    && matches!(error.error.raw_os_error(), Some(5 | 32 | 33))
                {
                    staged = error.file;
                    std::thread::sleep(std::time::Duration::from_millis(10 << attempt));
                    attempt += 1;
                } else {
                    return Err(error.error)
                        .with_context(|| format!("failed replacing file {}", path.display()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_parents_and_replaces_complete_contents() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing/parents/config.json");
        write(&path, b"original").unwrap();
        write(&path, b"replacement").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn failed_replacement_preserves_destination_and_removes_staged_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("directory");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("original"), b"retained").unwrap();
        let error = write(&path, b"replacement").unwrap_err();
        assert_eq!(fs::read(path.join("original")).unwrap(), b"retained");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
        drop(error);
    }

    #[cfg(windows)]
    #[test]
    fn retries_a_temporary_windows_sharing_conflict() {
        use std::{
            os::windows::fs::OpenOptionsExt,
            sync::{Arc, Barrier},
            thread,
            time::Duration,
        };
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        fs::write(&path, b"original").unwrap();
        let mut staged = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
        staged.write_all(b"replacement").unwrap();
        // Simulate a scanner that permits reads/writes but briefly denies deletion.
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let writer_barrier = barrier.clone();
        let target = path.clone();
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            replace_staged(staged, &target)
        });
        barrier.wait();
        thread::sleep(Duration::from_millis(30));
        drop(reader);
        writer.join().unwrap().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"replacement");
    }
}
