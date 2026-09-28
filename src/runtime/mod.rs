//! Process facts, liveness, terminal multiplexers, and health checks.

pub mod doctor;
pub mod insert;
pub mod live;
pub(crate) mod procfs;
pub mod resume;
pub(crate) mod tmux;
