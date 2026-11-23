//! E08 resource arbitration. Network enrollment, OS peer authentication and GPU
//! probing are separate boundaries; no socket service is exposed by this crate yet.
pub mod broker;
pub mod control;
pub mod gpu_inventory;
pub mod node_journal;
mod probe_command;
pub mod wsl_inventory;

pub mod execution_journal;

#[cfg(unix)]
pub mod cpu;
#[cfg(unix)]
pub mod host;
