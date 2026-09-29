#![allow(clippy::missing_errors_doc, clippy::must_use_candidate)]

pub mod analysis;
pub mod cache;
pub mod evidence;
pub mod frontend;
mod frontend_lowering;
pub mod ids;
pub mod ir;
pub mod link;
pub mod models;
pub mod project;
pub mod queries;
mod solver;

pub use analysis::Analyzer;
pub use project::{Project, ProjectConfig};
