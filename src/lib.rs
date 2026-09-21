//! A Matrix client and ACP client for separately provisioned agent workers.
//! The live launcher is feature gated; offline fixtures remain deterministic.

pub mod acp;
pub mod config;
pub mod core;
#[cfg(feature = "matrix")]
pub mod live;
#[cfg(feature = "matrix")]
pub mod matrix;
pub mod model;
pub mod offline;
pub mod runner;
pub mod setup;
pub mod store;
#[cfg(feature = "matrix")]
mod trust;
