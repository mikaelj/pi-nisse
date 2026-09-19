//! Core domain primitives.
//!
//! Types defined here are shared across the Nisse core. All serialized forms
//! are strict: unknown enum variants and malformed values are rejected.

pub mod context;
pub mod evidence;
pub mod finding;
pub mod ids;
pub mod jury;
pub mod model;
pub mod practice;
pub mod records;
pub mod report;
pub mod review;
pub mod reviewer;
pub mod routing;
pub mod snapshot;
pub mod time;

/// Durable record schema version produced by this build.
pub const SCHEMA_VERSION: u32 = 1;
