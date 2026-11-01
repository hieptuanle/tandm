//! tandm core: everything except the window. The CLI and the GUI are thin layers over this.

pub mod app;
pub mod config;
pub mod engine;
pub mod git;
pub mod ops;
pub mod paths;
pub mod remote;
pub mod secrets;
pub mod service;
pub mod state;
pub mod status;
pub mod util;

pub use config::{Config, MachineConfig, RepoConfig};
pub use state::{RunRecord, RunResult, SkipReason};
pub use status::{CellState, MachineStatus, RepoStatus, Tone};
