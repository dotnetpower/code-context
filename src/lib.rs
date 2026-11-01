pub mod catalog;
pub mod context;
pub mod daemon;
pub mod diagnostics;
pub mod document;
pub mod engine;
pub mod exclusion_policy;
pub mod experiment;
pub mod files;
pub mod incremental;
pub mod mcp;
pub mod memory;
pub mod outcome;
pub mod planner;
pub mod policy;
pub mod query;
pub mod registration;
pub mod resident;
pub mod resident_experiment;
pub mod search;
pub mod shared_mcp;
pub mod watched;
pub mod workspace;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}
