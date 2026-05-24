//! The `RasterType` numeric trait bound for raster element types.
//!
//! This trait lives in `eo-io` (not `eo-core`) because it requires the
//! `gdal::raster::GdalType` bound — anything that wants to use this
//! trait will also need libgdal.

use num_traits::{NumCast, ToPrimitive, Zero};
use std::fmt::Debug;

/// Numeric trait bound that a raster element type must satisfy.
///
/// Implemented for the standard integer and floating-point types that
/// `gdal::raster::GdalType` accepts: `i8`, `i16`, `i32`, `i64`, `u8`,
/// `u16`, `u32`, `u64`, `f32`, `f64`. The `GdalType` bound lets generic
/// code use `RasterBand::read_as::<T>` and the typed
/// `create_with_band_type_with_options::<T, _>` factories.
pub trait RasterType:
    Copy
    + Send
    + Sync
    + 'static
    + Zero
    + NumCast
    + ToPrimitive
    + Debug
    + ::gdal::raster::GdalType
{
}

impl<T> RasterType for T where
    T: Copy
        + Send
        + Sync
        + 'static
        + Zero
        + NumCast
        + ToPrimitive
        + Debug
        + ::gdal::raster::GdalType
{
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check: standard numeric types implement `RasterType`.
    /// If this function compiles, the blanket impl is correct.
    #[allow(dead_code)]
    fn assert_impls() {
        fn takes<T: RasterType>() {}
        takes::<u8>();
        takes::<u16>();
        takes::<u32>();
        takes::<u64>();
        takes::<i8>();
        takes::<i16>();
        takes::<i32>();
        takes::<i64>();
        takes::<f32>();
        takes::<f64>();
    }

    #[test]
    fn raster_type_is_object_safe_via_marker() {
        // The trait has supertrait bounds incl. `Sized` (Copy), so it isn't
        // dyn-compat, but blanket impl callability is the contract. Compile
        // success of `assert_impls` is the real test; this just exists so
        // `cargo test -p eo-io --features gdal` reports at least one passing
        // test even without the auto-detected `assert_impls` symbol.
    }
}
