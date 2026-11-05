//! E08 resource arbitration. Network enrollment, OS peer authentication and GPU
//! probing are separate boundaries; no socket service is exposed by this crate yet.
pub mod broker;
pub mod control;
pub mod node_journal;

pub mod execution_journal;
