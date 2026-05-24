//! Output-format configuration for raster writers.

use crate::block::BlockSize;
use serde::{Deserialize, Serialize};

/// Output format for `apply*` methods.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutputFormat {
    /// Standard GeoTIFF with LZW + overviews. Default.
    #[default]
    GeoTiff,
    /// Cloud-Optimized GeoTIFF (tile-aligned, internal overviews).
    Cog,
    /// Virtual raster (no pixel data, just metadata).
    Vrt,
}

impl OutputFormat {
    /// GDAL driver short name corresponding to this format.
    #[must_use]
    pub const fn gdal_driver(self) -> &'static str {
        match self {
            Self::GeoTiff => "GTiff",
            Self::Cog => "COG",
            Self::Vrt => "VRT",
        }
    }
}

/// Configuration knobs for raster output.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutputConfig {
    /// Output format / driver.
    pub format: OutputFormat,
    /// Compression option (e.g. "LZW", "DEFLATE", "ZSTD", "NONE").
    pub compression: String,
    /// Tile size for tiled output.
    pub tile_size: BlockSize,
    /// Overview pyramid levels (empty = no overviews).
    pub overview_levels: Vec<i32>,
    /// Overview resampling method (e.g. "AVERAGE", "GAUSS", "NEAREST").
    pub overview_resampling: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: OutputFormat::default(),
            compression: "LZW".into(),
            tile_size: BlockSize::new(512, 512),
            overview_levels: vec![2, 4, 8, 16],
            overview_resampling: "AVERAGE".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdal_driver_strings_match_gdal() {
        assert_eq!(OutputFormat::GeoTiff.gdal_driver(), "GTiff");
        assert_eq!(OutputFormat::Cog.gdal_driver(), "COG");
        assert_eq!(OutputFormat::Vrt.gdal_driver(), "VRT");
    }

    #[test]
    fn default_format_is_geotiff() {
        assert_eq!(OutputFormat::default(), OutputFormat::GeoTiff);
    }

    #[test]
    fn default_config_has_sane_values() {
        let cfg = OutputConfig::default();
        assert_eq!(cfg.format, OutputFormat::GeoTiff);
        assert_eq!(cfg.compression, "LZW");
        assert_eq!(cfg.tile_size, BlockSize::new(512, 512));
        assert_eq!(cfg.overview_levels, vec![2, 4, 8, 16]);
        assert_eq!(cfg.overview_resampling, "AVERAGE");
    }

    #[test]
    fn serde_roundtrip_output_config() {
        let cfg = OutputConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: OutputConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn output_format_serde_uses_variant_name() {
        let json = serde_json::to_string(&OutputFormat::Cog).unwrap();
        assert!(json.contains("Cog"));
    }
}
