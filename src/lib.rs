//! Nisse core library.
//!
//! The library carries the shared surface used by the `nisse` binary and by
//! integration tests. Deterministic decisions and durable state belong here.

pub mod app;
pub mod cli;
pub mod config;
pub mod domain;
pub mod git;
pub mod index;
pub mod jury;
pub mod learning;
pub mod mcp;
pub mod providers;
pub mod retrieval;
pub mod routing;
pub mod storage;

// Performance measurements and regression bounds are test-only (plan
// milestone 29); they use the crate-internal Git fixtures.
#[cfg(test)]
mod perf;
