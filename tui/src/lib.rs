//! statie-tui: ratatui frontend, `StepSink` impl, slash commands.
//!
//! Two entry points share the same driver engine (`driver::run_driver`):
//! `run_tui` (ratatui + crossterm) and `run_headless` (plain-text
//! transcript, for scripting and tests).

mod app;
mod driver;
mod error;
mod headless;
mod registry;
mod ui;

pub use app::run_tui;
pub use driver::{
    ChannelSink, Command, DriverCmd, Query, UiEvent, parse_command, render_entry,
    render_entry_line, render_patches, render_state, run_driver,
};
pub use error::TuiError;
pub use headless::{HeadlessSummary, run_headless};
pub use registry::{DynSkill, RunOptions, SkillRegistry};
