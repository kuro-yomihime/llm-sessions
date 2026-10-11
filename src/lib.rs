//! Workspace-first explorer for local LLM coding-agent sessions.
//!
//! Sessions from every installed agent are normalized into one
//! [`model::SessionSummary`] stream, keyed by the workspace they ran in
//! rather than by the tool that wrote them.

pub mod catalog;
pub mod clock;
pub mod error;
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
