//! **eo-catalog** — STAC catalog client + asset signers.
//!
//! This crate defines the *public API surface* for catalog access. Concrete
//! clients (`PlanetaryComputerClient`, `EarthSearchClient`, etc.) land in
//! later weeks. Today's scaffold lets dependent crates compile against the
//! stable traits.
//!
//! Public references:
//! - STAC API spec 1.0.0
//! - Planetary Computer Data Auth docs (SAS tokens)
//! - Element84 Earth Search docs (anonymous + signed S3)
//! - NASA EarthData URS (OIDC Bearer)

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod auth;
pub mod client;
pub mod provider;

pub use auth::{AssetSigner, AuthError};
pub use client::{SearchRequest, StacClient};
pub use provider::Provider;
