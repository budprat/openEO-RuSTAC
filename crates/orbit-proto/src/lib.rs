//! Generated proto bindings for orbit.etl.v1.
//!
//! The build script regenerates the module on every `cargo build` whenever
//! `proto/etl.proto` changes.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![allow(clippy::pedantic, clippy::nursery)]

pub mod etl {
    pub mod v1 {
        tonic::include_proto!("orbit.etl.v1");
    }
}

pub use etl::v1::*;
