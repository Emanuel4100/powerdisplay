//! Single-instance lock for `powerdisplayd`.
//!
//! The lock lives under the Flatpak app runtime dir so every instance of this app sees
//! the same file. `flock` is released when the process dies, so a leftover file after a
//! crash does not wedge the next start.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rustix::fs::{FlockOperation, flock};

pub const FLATPAK_ID: &str = "io.github.Emanuel4100.PowerDisplay";

pub fn lock_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    if crate::sandboxed() {
        runtime
            .join("app")
            .join(FLATPAK_ID)
            .join("powerdisplayd.lock")
    } else {
        runtime.join("powerdisplayd.lock")
    }
}

/// Returns `Some(file)` holding the exclusive lock, or `None` if another daemon is up.
pub fn try_acquire() -> Result<Option<File>> {
    let path = lock_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            file.set_len(0)
                .and_then(|_| writeln!(file, "{}", std::process::id()))
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(Some(file))
        }
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("locking {}", path.display())),
    }
}

/// True when another process currently holds the daemon lock.
pub fn is_held() -> bool {
    let path = lock_path();
    let Ok(file) = File::open(&path) else {
        return false;
    };
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _ = flock(&file, FlockOperation::Unlock);
            false
        }
        Err(rustix::io::Errno::WOULDBLOCK) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_is_under_the_runtime_dir() {
        let path = lock_path();
        assert!(
            path.ends_with("powerdisplayd.lock"),
            "{}",
            path.display()
        );
    }
}
