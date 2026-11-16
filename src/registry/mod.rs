//! Canonical national constants, state/FIPS/county lookup tables, FHFA loan limits,
//! regional construction/insurance/tax/transfer-tax schedules, and upstream portal registries.
//!
//! Centralizes domain tables previously duplicated across `property/*`, `modules/*`, `cre/*`,
//! `scanner/*`, `search/*`, `ai/*`, and `guardrails.rs`.

pub mod endpoints;
pub mod loan_limits;
pub mod portals;
pub mod rates_and_costs;
pub mod us_geo;
pub mod utilities;

pub use endpoints::*;
pub use loan_limits::*;
pub use portals::*;
pub use rates_and_costs::*;
pub use us_geo::*;
pub use utilities::*;
