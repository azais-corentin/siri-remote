//! Persistence for touchpad [`Calibration`]. IO-only — the data type
//! and the live calibration state machine live in [`super::state`].
//!
//! Resolution order for the config file:
//!   1. `$XDG_CONFIG_HOME/siri-remote/calibration.toml`, if set,
//!   2. `$HOME/.config/siri-remote/calibration.toml`.
//!
//! Loading is best-effort: a missing file returns `Ok(None)` (the
//! expected case on first launch), while a malformed file surfaces as
//! `Err` so the caller can `log::warn!` and fall through to defaults.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::state::Calibration;

const CONFIG_DIR: &str = "siri-remote";
const CONFIG_FILE: &str = "calibration.toml";

/// Resolve `$XDG_CONFIG_HOME` or `$HOME/.config`. Returns `None` only
/// when neither env var is set, in which case the caller MUST treat
/// calibration as session-only.
pub fn config_base() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    std::env::var_os("HOME").map(|home| {
        let mut p = PathBuf::from(home);
        p.push(".config");
        p
    })
}

/// Full file path for the calibration TOML, given a resolved config base.
pub fn config_path_in(base: &Path) -> PathBuf {
    base.join(CONFIG_DIR).join(CONFIG_FILE)
}

/// Load calibration from the given base. Returns `Ok(None)` when the
/// file is absent.
pub fn load_in(base: &Path) -> Result<Option<Calibration>> {
    let path = config_path_in(base);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", path.display()));
        }
    };
    let c: Calibration = toml::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(c))
}

/// Resolve the config base and load. Logs a warning and returns `None`
/// if the base cannot be resolved.
pub fn load() -> Option<Calibration> {
    let base = match config_base() {
        Some(b) => b,
        None => {
            log::warn!("XDG_CONFIG_HOME and HOME both unset; calibration is session-only");
            return None;
        }
    };
    match load_in(&base) {
        Ok(Some(c)) => Some(c),
        Ok(None) => None,
        Err(err) => {
            log::warn!("failed to load calibration: {err:#}");
            None
        }
    }
}

/// Save calibration to `<base>/siri-remote/calibration.toml`. Creates
/// the parent directory lazily.
pub fn save_in(base: &Path, c: &Calibration) -> Result<()> {
    let path = config_path_in(base);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = toml::to_string(c).context("serializing calibration")?;
    fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Resolve the config base and save. Returns `Err` if base is unresolvable
/// or the write fails.
pub fn save(c: &Calibration) -> Result<()> {
    let base = config_base().context("no XDG_CONFIG_HOME or HOME set")?;
    save_in(&base, c)
}

/// Delete the calibration file under `base`. Missing file is not an error.
pub fn clear_in(base: &Path) -> Result<()> {
    let path = config_path_in(base);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

/// Resolve the config base and clear. Missing base is treated as a no-op.
pub fn clear() -> Result<()> {
    if let Some(base) = config_base() {
        clear_in(&base)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_base() -> PathBuf {
        let pid = std::process::id();
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("siri-remote-cal-test-{pid}-{id}"));
        // Make sure we start clean.
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn load_missing_returns_none() {
        let base = temp_base();
        assert!(load_in(&base).unwrap().is_none());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let base = temp_base();
        let c = Calibration { x_origin: 120, x_span: 1820, y_min: 8, y_max: 96 };
        save_in(&base, &c).unwrap();
        let loaded = load_in(&base).unwrap().expect("calibration present after save");
        assert_eq!(loaded, c);
        // Cleanup.
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn clear_removes_file_and_is_idempotent() {
        let base = temp_base();
        let c = Calibration::default();
        save_in(&base, &c).unwrap();
        assert!(config_path_in(&base).exists());
        clear_in(&base).unwrap();
        assert!(!config_path_in(&base).exists());
        // Second clear is a no-op.
        clear_in(&base).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn malformed_file_is_err() {
        let base = temp_base();
        let path = config_path_in(&base);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not = valid = toml = at = all").unwrap();
        assert!(load_in(&base).is_err());
        let _ = fs::remove_dir_all(&base);
    }
}
