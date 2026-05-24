//! VSI path rewriting — convert remote asset URLs into the form GDAL's
//! Virtual File System layer can read directly.
//!
//! GDAL's VSI (Virtual Systems Interface) accepts pseudo-paths like
//! `/vsicurl/https://...` and `/vsis3/bucket/key`. Most STAC catalogs hand
//! out plain `https://` or `s3://` URLs; we translate them at the edge so
//! the rest of the pipeline treats every asset as "a local-looking path".
//!
//! References:
//! - <https://gdal.org/user/virtual_file_systems.html>
//! - <https://github.com/radiantearth/stac-spec/blob/main/best-practices.md#asset-href-best-practices>

/// Convert a remote asset URL into a GDAL VSI path.
///
/// Mappings:
///
/// | Input scheme | Output |
/// |---|---|
/// | `s3://bucket/key` | `/vsis3/bucket/key` |
/// | `gs://bucket/key` | `/vsigs/bucket/key` |
/// | `az://account/container/key` | `/vsiaz/account/container/key` |
/// | `https://…` / `http://…` | `/vsicurl/<full-url>` |
/// | `/vsicurl/…`, `/vsis3/…`, `/vsigs/…`, `/vsiaz/…` | unchanged (already VSI) |
/// | Local path / unknown scheme | unchanged |
///
/// All translations are syntactic — no I/O is performed.
#[must_use]
pub fn vsi_rewrite(url: &str) -> String {
    if url.starts_with("/vsi") {
        return url.to_string();
    }
    if let Some(rest) = url.strip_prefix("s3://") {
        return format!("/vsis3/{rest}");
    }
    if let Some(rest) = url.strip_prefix("gs://") {
        return format!("/vsigs/{rest}");
    }
    if let Some(rest) = url.strip_prefix("az://") {
        return format!("/vsiaz/{rest}");
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return format!("/vsicurl/{url}");
    }
    // Local path or unknown scheme — leave alone.
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_becomes_vsicurl() {
        let url = "https://example.com/data.tif";
        assert_eq!(vsi_rewrite(url), "/vsicurl/https://example.com/data.tif");
    }

    #[test]
    fn http_becomes_vsicurl() {
        let url = "http://example.com/data.tif";
        assert_eq!(vsi_rewrite(url), "/vsicurl/http://example.com/data.tif");
    }

    #[test]
    fn s3_protocol_becomes_vsis3() {
        let url = "s3://my-bucket/path/to/data.tif";
        assert_eq!(vsi_rewrite(url), "/vsis3/my-bucket/path/to/data.tif");
    }

    #[test]
    fn gs_protocol_becomes_vsigs() {
        assert_eq!(
            vsi_rewrite("gs://my-bucket/scene.tif"),
            "/vsigs/my-bucket/scene.tif"
        );
    }

    #[test]
    fn az_protocol_becomes_vsiaz() {
        assert_eq!(
            vsi_rewrite("az://account/container/blob.tif"),
            "/vsiaz/account/container/blob.tif"
        );
    }

    #[test]
    fn already_vsi_passthrough() {
        for already in [
            "/vsicurl/https://example.com/a.tif",
            "/vsis3/bucket/key",
            "/vsigs/bucket/key",
            "/vsiaz/account/container/key",
        ] {
            assert_eq!(vsi_rewrite(already), already);
        }
    }

    #[test]
    fn local_path_unchanged() {
        let p = "/tmp/data/scene.tif";
        assert_eq!(vsi_rewrite(p), p);
    }

    #[test]
    fn relative_path_unchanged() {
        let p = "data/scene.tif";
        assert_eq!(vsi_rewrite(p), p);
    }

    #[test]
    fn empty_unchanged() {
        assert_eq!(vsi_rewrite(""), "");
    }

    #[test]
    fn unknown_scheme_unchanged() {
        // Future-proofing: ftp:// etc. — pass through rather than mangle.
        assert_eq!(vsi_rewrite("ftp://example.com/x"), "ftp://example.com/x");
    }
}
