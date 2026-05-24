//! **eo-mask** — quality-assessment masks for satellite imagery.
//!
//! Public surface lands here in Week 6; algorithm bodies land subsequently
//! from their public algorithmic descriptions:
//!
//! - Fmask 4.x — Zhu, Wang & Woodcock (2015)
//! - s2cloudless — sentinel-hub/sentinel2-cloud-detector (Apache-2.0 port)
//! - SCL decoder — Sentinel-2 Product Definition (lookup table)
//! - QA_PIXEL decoder — USGS Landsat C2 Product Guide (bitfield)
//!
//! See `docs/clean-room-protocol.md` for the discipline that keeps this
//! crate independent of the upstream raster engine workspace.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod confidence;
pub mod provider;
pub mod qa_pixel;
pub mod scl;

pub use confidence::MaskConfidence;
pub use provider::{CloudMaskProvider, MaskBand, MaskValue};
pub use qa_pixel::{QaConfidence, QaPixel};
pub use scl::ScenSceneClass;
