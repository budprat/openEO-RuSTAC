//! Execution modes for `ImageQuery` — how to actually fetch / locate the
//! imagery once a query has been built.

use crate::dsl::ImageQuery;
use crate::providers::vsi_rewrite;

impl ImageQuery {
    /// Convert a list of HTTP(S)/S3 asset hrefs (from a STAC search response)
    /// into GDAL VSI paths suitable for direct opening. No download — GDAL
    /// reads via range requests at open time.
    ///
    /// Returns the VSI-rewritten asset paths in input order.
    pub fn get_remote(&self, asset_hrefs: &[String]) -> Vec<String> {
        asset_hrefs.iter().map(|h| vsi_rewrite(h)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::{Collection, ImageQueryBuilder, Intersects};
    use crate::providers::Provider;

    fn minimal_query() -> ImageQuery {
        ImageQueryBuilder::new()
            .provider(Provider::EARTH_SEARCH_V1)
            .collection(Collection::Sentinel2)
            .intersects(Intersects::Bbox([148.0, -29.0, 149.0, -28.0]))
            .build()
            .unwrap()
    }

    /// **RED T2.6/A1**: get_remote rewrites HTTPS to `/vsicurl/`.
    #[test]
    fn get_remote_rewrites_https_hrefs() {
        let q = minimal_query();
        let hrefs = vec![
            "https://sentinel-cogs.s3.us-west-2.amazonaws.com/B04.tif".to_string(),
            "https://sentinel-cogs.s3.us-west-2.amazonaws.com/B08.tif".to_string(),
        ];
        let result = q.get_remote(&hrefs);
        assert_eq!(result.len(), 2, "should preserve count");
        assert!(result[0].starts_with("/vsicurl/"), "HTTPS → /vsicurl/");
        assert!(result[1].starts_with("/vsicurl/"), "HTTPS → /vsicurl/");
    }

    /// **RED T2.6/A2**: get_remote rewrites s3:// to `/vsis3/`.
    #[test]
    fn get_remote_rewrites_s3_hrefs() {
        let q = minimal_query();
        let hrefs = vec!["s3://sentinel-cogs/foo/B04.tif".to_string()];
        let result = q.get_remote(&hrefs);
        assert_eq!(result.len(), 1);
        assert!(result[0].starts_with("/vsis3/"), "s3:// → /vsis3/");
    }
}
