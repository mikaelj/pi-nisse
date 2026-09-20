//! Deterministic learning: turning review outcomes into durable project
//! knowledge without trusting reviewer claims.
//!
//! - [`facts`]: fact candidates and independent verification (plan
//!   sections 46–48; milestone 18).
//! - [`feedback`]: known finding outcomes as immutable observations
//!   (plan sections 49–50; milestone 19).
//!
//! Lesson learning and knowledge consolidation are later milestones.

pub mod facts;
pub mod feedback;
