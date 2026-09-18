//! Terminal adapter shared by the CLI and local examples.
pub mod control;
pub mod error;
pub mod terminal;
pub use error::{Error, Result};
