//! deskpilot core: everything that does not depend on the operating system.
//!
//! An OS adapter implements [`platform::Platform`]; the binary hands it to
//! [`cli::run`], which serves MCP, runs one-shot CLI calls or acts as the
//! elevated worker.

pub mod cli;
pub mod dispatch;
pub mod engine;
pub mod error;
pub mod imaging;
pub mod keys;
pub mod mcp;
pub mod model;
pub mod platform;
pub mod policy;
pub mod query;
pub mod render;
pub mod tools;
pub mod worker;

pub use error::{Error, Result};
