//! Phase-C #5 — storage contracts.
//!
//! - COG validation (signature check + tile layout)
//! - STAC Item generator wrapping a saved asset + `ProvenanceRecord`
//! - Output-format taxonomy that the runner consults

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::provenance::ProvenanceRecord;

/// Errors the storage contracts layer can surface.
#[derive(Debug, Error, PartialEq)]
pub enum StorageError {
    /// Bytes don't carry a TIFF magic header.
    #[error("not a TIFF: first 4 bytes = {0:?}")]
    NotTiff([u8; 4]),
    /// File is shorter than the minimum TIFF header + first IFD.
    #[error("TIFF too short: {0} bytes")]
    TooShort(usize),
}

/// Supported output formats (canonical names from openEO spec).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum StorageFormat {
    /// GeoTIFF (uncompressed or LZW).
    GTiff,
    /// Cloud-Optimized GeoTIFF (subset of GeoTIFF; tile-based).
    COG,
    /// NetCDF — deferred, currently emitted as TIFF.
    NetCDF,
    /// JSON (numeric / metadata outputs).
    JSON,
    /// PNG — visualisation 8-bit grayscale.
    PNG,
}

impl StorageFormat {
    /// IANA media type for the produced bytes.
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Self::GTiff | Self::COG | Self::NetCDF => "image/tiff",
            Self::JSON => "application/json",
            Self::PNG => "image/png",
        }
    }
}

/// Validate that `bytes` start with a valid TIFF magic header.
pub fn validate_tiff_bytes(bytes: &[u8]) -> Result<(), StorageError> {
    if bytes.len() < 8 {
        return Err(StorageError::TooShort(bytes.len()));
    }
    let magic: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if &magic != b"II*\0" && &magic != b"MM\0*" {
        return Err(StorageError::NotTiff(magic));
    }
    Ok(())
}

/// Validate basic COG structure — TIFF magic check today; tile-size
/// + IFD-order checks land when we integrate a real TIFF parser.
pub fn validate_cog_bytes(bytes: &[u8]) -> Result<(), StorageError> {
    validate_tiff_bytes(bytes)?;
    Ok(())
}

/// Generate a minimal STAC Item JSON wrapping an asset + provenance.
#[must_use]
pub fn stac_item_for_asset(
    item_id: &str,
    asset_name: &str,
    asset_href: &str,
    media_type: &str,
    prov: &ProvenanceRecord,
) -> Value {
    json!({
        "type": "Feature",
        "stac_version": "1.0.0",
        "id": item_id,
        "geometry": null,
        "properties": {
            "datetime": prov.processed_at,
            "orbit:graph_hash": prov.graph_hash,
            "orbit:software_version": prov.software_version,
            "orbit:source_stac_ids": prov.stac_ids,
        },
        "assets": {
            asset_name: {
                "href": asset_href,
                "type": media_type,
                "roles": ["data"],
                "orbit:band_mapping": prov.band_mapping,
                "orbit:nodata": prov.nodata,
                "orbit:scale": prov.scale,
                "orbit:offset": prov.offset,
            }
        },
        "links": []
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_tiff_accepts_little_endian_magic() {
        let mut b = vec![b'I', b'I', b'*', 0u8];
        b.resize(16, 0);
        assert!(validate_tiff_bytes(&b).is_ok());
    }

    #[test]
    fn validate_tiff_accepts_big_endian_magic() {
        let mut b = vec![b'M', b'M', 0u8, b'*'];
        b.resize(16, 0);
        assert!(validate_tiff_bytes(&b).is_ok());
    }

    #[test]
    fn validate_tiff_rejects_other_bytes() {
        let b = [0u8; 16];
        match validate_tiff_bytes(&b) {
            Err(StorageError::NotTiff(m)) => assert_eq!(m, [0, 0, 0, 0]),
            other => panic!("expected NotTiff, got {other:?}"),
        }
    }

    #[test]
    fn validate_tiff_rejects_short_buffer() {
        let b = [b'I', b'I', b'*'];
        assert!(matches!(validate_tiff_bytes(&b), Err(StorageError::TooShort(3))));
    }

    #[test]
    fn cog_validation_passes_basic_tiff_today() {
        let mut b = vec![b'I', b'I', b'*', 0u8];
        b.resize(64, 0);
        assert!(validate_cog_bytes(&b).is_ok());
    }

    #[test]
    fn storage_format_media_types_match_expectation() {
        assert_eq!(StorageFormat::GTiff.media_type(), "image/tiff");
        assert_eq!(StorageFormat::COG.media_type(), "image/tiff");
        assert_eq!(StorageFormat::JSON.media_type(), "application/json");
        assert_eq!(StorageFormat::PNG.media_type(), "image/png");
        assert_eq!(StorageFormat::NetCDF.media_type(), "image/tiff");
    }

    #[test]
    fn storage_format_serialises_uppercase() {
        let j = serde_json::to_string(&StorageFormat::GTiff).unwrap();
        assert_eq!(j, "\"GTIFF\"");
    }

    #[test]
    fn stac_item_wraps_asset_with_provenance_fields() {
        let mut p = ProvenanceRecord::new("h0", "orbit-openeo/0.1");
        p.stac_ids.push("S2A_x".into());
        p.band_mapping.insert("red".into(), "B04".into());
        p.nodata.insert("red".into(), 0.0);
        let item = stac_item_for_asset(
            "job-0001",
            "result.tif",
            "/jobs/job-0001/results/result.tif",
            "image/tiff",
            &p,
        );
        assert_eq!(item["type"], "Feature");
        assert_eq!(item["id"], "job-0001");
        assert_eq!(item["properties"]["orbit:graph_hash"], "h0");
        assert_eq!(item["properties"]["orbit:source_stac_ids"][0], "S2A_x");
        let a = &item["assets"]["result.tif"];
        assert_eq!(a["href"], "/jobs/job-0001/results/result.tif");
        assert_eq!(a["type"], "image/tiff");
        assert_eq!(a["orbit:band_mapping"]["red"], "B04");
    }
}
