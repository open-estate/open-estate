#![recursion_limit = "512"]
pub mod extensions;
pub mod registry;
pub mod shared;
pub mod proxy_pool;
pub mod scrape;
pub mod guardrails;
pub mod db;
pub mod ai;
pub mod cre; // [feat/cre-mode]
pub mod search;
pub mod property;
pub mod modules;
pub use modules::routing_providers;
pub mod scanner;
pub mod scoring;
pub mod web;

#[cfg(test)]
extern crate self as open_estate;
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
pub(crate) mod test_synth;
