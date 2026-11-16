//! "Your Match": personalized, deterministic, explainable scoring over eleven dimensions.
//!
//! Design: implementation plan v3, sections 5.2–5.5 and 5.12; a faithful port of
//! `match_prototype_v3.py`.
//!
//! - [`catalog`]: dimensions, the sixteen style families and the RESO `ArchitecturalStyle` mapping,
//!   screens, features and every label in English, Simplified Chinese and Spanish; [`catalog_json`].
//! - [`presets`]: the ten lifestyle presets, identical in every language.
//! - [`profile`]: the versioned buyer profile and [`Profile::validate`].
//! - [`factors`]: [`HomeFactors::from_report`], every scoring input with its provenance.
//! - [`utility`]: the documented 0–100 curves.
//! - [`engine`]: [`rank`]: imputation, coverage, gates, critical caps and ties.
//! - [`explain`]: reasons and trade-offs rendered from templates.
//!
//! Scores are computed here and nowhere else: the LLM never produces a number, and no resident or
//! demographic data is read (plan section 6).

pub mod catalog;
pub mod engine;
pub mod explain;
pub mod factors;
pub mod presets;
pub mod profile;
pub mod utility;

pub use catalog::{catalog_json, Dimension, Lang, StyleFamily};
pub use engine::{rank, rank_with, CommuteMatrix, RankOptions, RankResult, ScoredHome};
pub use factors::HomeFactors;
pub use profile::Profile;
