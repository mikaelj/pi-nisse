//! Jury composition.
//!
//! This milestone provides the bundled reviewer catalog and practice packs
//! ([`catalog`]). Jury plan construction from routing decisions is
//! implemented in a later milestone.

pub mod catalog;

pub use catalog::{Catalog, CatalogError};
