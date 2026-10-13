//! Extracts the Project SEKAI app identity (app version, app hash, data
//! version and the rest of the client's `PlayerSettingConfig`) from published
//! game packages, reading only the parts of the package it needs.
//!
//! The pipeline: [`remote`] serves a package over HTTP range requests as a
//! `unity-rs` [`Region`](unity_rs_core::source::Region); [`zip`] walks it
//! (including stored APKs inside an XAPK); [`unity`] opens the Unity data with
//! `unity-rs` and decodes the config objects ([`player_settings`]);
//! [`identity`] turns that into the reported [`identity::AppIdentity`];
//! [`runner`] adds caching and [`publish`]ing; [`service`] schedules passes.

pub mod axml;
pub mod bundle_source;
pub mod config;
pub mod error;
pub mod extract;
pub mod identity;
pub mod logging;
pub mod player_settings;
pub mod publish;
pub mod qooapp;
pub mod remote;
pub mod runner;
pub mod server;
pub mod service;
pub mod state;
pub mod testing;
pub mod unity;
pub mod zip;

pub use error::{Error, Result};
pub use server::ServerRegion;
