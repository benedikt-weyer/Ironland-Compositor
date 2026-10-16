//! Resolves which file `ironlandctl` reads from and writes to.
//!
//! Reads always go through [`ironland_config::Config::try_load`]/
//! [`ironland_config::AppearanceSettings::load`], which already implement
//! the search-path-plus-defaults merge; a `--config <path>` override is
//! threaded through by pointing `$IRONLAND_COMPOSITOR_CONFIG` at it for the
//! lifetime of the process (the same env var those functions already
//! consult as their highest-priority override), rather than re-implementing
//! that merge here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Points reads at `path` instead of the normal search path, for the rest of
/// this process. Safe to call once, early in `main`, before any other
/// thread exists.
pub fn apply_read_override(path: &Path) {
    // SAFETY: called once at start-up, before any thread that might read
    // the environment concurrently is spawned.
    unsafe { std::env::set_var("IRONLAND_COMPOSITOR_CONFIG", path) };
}

/// Where a `set`/`unset`/`shortcuts`/`outputs`/`apply` write should land: an
/// explicit `--config` override if given, else the user's own config path
/// (never the system-wide `/etc` file, even if that's what's currently
/// shadowing the user's file for reads - matching the settings GUI, which
/// only ever saves to the user path).
pub fn write_path(explicit: &Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.clone());
    }
    ironland_config::user_config_path()
        .context("cannot determine a config path: neither $XDG_CONFIG_HOME nor $HOME is set")
}

/// Atomically writes `contents` to `path`, creating its parent directory if
/// needed. Atomic replacement ensures the compositor's live-reload watcher
/// never observes a partially written file.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let tmp_path = dir.join(format!(
        ".{}.tmp.{unique}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml")
    ));

    std::fs::write(&tmp_path, contents)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("replacing {}", path.display())
    })?;
    Ok(())
}

/// Reads `path` as a [`toml_edit::DocumentMut`], defaulting to an empty
/// document if it doesn't exist yet.
pub fn load_doc(path: &Path) -> Result<toml_edit::DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(contents) => contents
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parsing {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(toml_edit::DocumentMut::new()),
        Err(err) => bail!("reading {}: {err}", path.display()),
    }
}
