//! A Matrix client and ACP client for separately provisioned agent workers.
//! The live launcher is feature gated; offline fixtures remain deterministic.

pub mod acp;
pub mod config;
pub mod context;
pub mod core;
#[cfg(feature = "matrix")]
pub mod live;
#[cfg(feature = "matrix")]
pub mod matrix;
#[cfg(feature = "matrix")]
pub mod matrix_tools;
pub mod messaging;
pub mod model;
pub mod offline;
pub mod runner;
pub mod setup;
pub mod store;
#[cfg(feature = "matrix")]
mod trust;

mod context_ledger;
