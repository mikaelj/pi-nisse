//! Deterministic learning: turning review outcomes into durable project
//! knowledge without trusting reviewer claims.
//!
//! - [`facts`]: fact candidates and independent verification (plan
//!   sections 46–48; milestone 18).
//! - [`feedback`]: known finding outcomes as immutable observations
//!   (plan sections 49–50; milestone 19).
//! - [`lessons`]: deterministic lesson learning from repeated
//!   observation outcomes (plan sections 43 and 51; milestone 20).
//! - [`staleness`]: fact staleness against the current snapshot and
//!   targeted revalidation (plan section 48; milestone 21).
//! - [`consolidation`]: knowledge consolidation into `.nisse/common`
//!   (plan sections 52–56; milestone 22).
//!
//! No LLM participates in learning statistics: Rust computes every
//! authoritative observation field, every lesson statistic, and every
//! routing condition; LLMs may at most contribute constrained
//! synthesis or wording that never controls behavior.

pub mod consolidation;
pub mod facts;
pub mod feedback;
pub mod lessons;
pub mod staleness;
