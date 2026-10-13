//! Workspace-first explorer for local LLM coding-agent sessions.
//!
//! Sessions from every installed agent are normalized into one
//! [`model::SessionSummary`] stream, keyed by the workspace they ran in
//! rather than by the tool that wrote them.

pub mod catalog;
pub mod clock;
pub mod error;
pub mod federation;
pub mod model;
pub mod output;
pub mod runtime;
#[cfg(feature = "serve")]
pub mod serve;
pub mod source;
pub mod storage;
pub mod tui;

pub use catalog::scan::{Scan, scan_with};
pub use error::{Error, Result};
pub use model::{Locator, SessionSummary, SourceId, Transcript, Usage};

pub fn home() -> Result<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or(Error::NoHome)
}

/// One XDG base directory: the environment override when absolute, else the
/// spec's default below `home`. Relative or empty overrides are ignored, as
/// the base-directory spec requires.
fn xdg_base(home: &std::path::Path, var: &str, default: &str) -> std::path::PathBuf {
    std::env::var_os(var)
        .map(std::path::PathBuf::from)
        .filter(|path: &std::path::PathBuf| path.is_absolute())
        .unwrap_or_else(|| home.join(default))
}

pub fn config_home(home: &std::path::Path) -> std::path::PathBuf {
    xdg_base(home, "XDG_CONFIG_HOME", ".config")
}

pub fn state_home(home: &std::path::Path) -> std::path::PathBuf {
    xdg_base(home, "XDG_STATE_HOME", ".local/state")
}

pub fn cache_home(home: &std::path::Path) -> std::path::PathBuf {
    xdg_base(home, "XDG_CACHE_HOME", ".cache")
}

/// Where agent stores live. Agents that follow the base-directory spec
/// (opencode, kilo, goose, devin) read this same variable, so their stores
/// must be resolved through it too, not pinned under `~/.local/share`.
pub fn data_home(home: &std::path::Path) -> std::path::PathBuf {
    xdg_base(home, "XDG_DATA_HOME", ".local/share")
}
