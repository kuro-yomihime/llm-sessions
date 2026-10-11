//! Process facts, liveness, terminal multiplexers, and health checks.

pub mod clipboard;
pub mod doctor;
pub mod insert;
pub mod live;
pub(crate) mod procfs;
pub mod resume;
pub mod status;
pub(crate) mod tmux;
