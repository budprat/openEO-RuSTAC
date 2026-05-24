//! Provider identifiers — which STAC backend a catalog refers to.

use serde::{Deserialize, Serialize};

/// Known STAC providers we plan to integrate with.
///
/// `Custom` carries an opaque endpoint so users can target self-hosted
/// catalogs. Concrete client implementations dispatch on this enum.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    /// Microsoft Planetary Computer (uses SAS-token signing).
    PlanetaryComputer,
    /// Element84 Earth Search (anonymous reads + optional S3 SigV4).
    EarthSearch,
    /// NASA EarthData CMR-STAC (OIDC Bearer).
    EarthData,
    /// USGS Landsat STAC.
    UsgsLandsat,
    /// User-supplied endpoint.
    Custom {
        /// STAC API root URL.
        endpoint: String,
    },
}

impl Provider {
    /// Default endpoint for this provider, when one exists.
    #[must_use]
    pub fn default_endpoint(&self) -> Option<&str> {
        match self {
            Self::PlanetaryComputer => Some("https://planetarycomputer.microsoft.com/api/stac/v1"),
            Self::EarthSearch => Some("https://earth-search.aws.element84.com/v1"),
            Self::EarthData => Some("https://cmr.earthdata.nasa.gov/stac"),
            Self::UsgsLandsat => Some("https://landsatlook.usgs.gov/stac-server"),
            Self::Custom { endpoint } => Some(endpoint.as_str()),
        }
    }

    /// Stable short identifier suitable for logs / metrics labels.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::PlanetaryComputer => "planetary-computer",
            Self::EarthSearch => "earth-search",
            Self::EarthData => "earth-data",
            Self::UsgsLandsat => "usgs-landsat",
            Self::Custom { .. } => "custom",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoints_are_https() {
        for p in [
            Provider::PlanetaryComputer,
            Provider::EarthSearch,
            Provider::EarthData,
            Provider::UsgsLandsat,
        ] {
            let ep = p.default_endpoint().unwrap();
            assert!(ep.starts_with("https://"), "{p:?} endpoint must be https");
        }
    }

    #[test]
    fn custom_endpoint_passthrough() {
        let p = Provider::Custom { endpoint: "http://localhost:8080".into() };
        assert_eq!(p.default_endpoint(), Some("http://localhost:8080"));
        assert_eq!(p.tag(), "custom");
    }

    #[test]
    fn tags_are_stable_kebab() {
        assert_eq!(Provider::PlanetaryComputer.tag(), "planetary-computer");
        assert_eq!(Provider::EarthSearch.tag(), "earth-search");
    }
}
