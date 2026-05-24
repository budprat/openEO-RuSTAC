//! Provider-portable satellite collection names.

/// Canonical satellite collection identifier — resolves to a provider-specific
/// collection ID via [`Collection::id_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Collection {
    /// Sentinel-2 L2A (surface reflectance, atmospherically corrected).
    Sentinel2,
    /// Landsat 8 / Landsat 9 — Collection 2 Level 2 SR.
    Landsat8,
    /// Landsat 7 — Collection 2 Level 2 SR.
    Landsat7,
    /// Landsat 5 — Collection 2 Level 2 SR.
    Landsat5,
}

impl Collection {
    /// Resolve to the provider's specific collection identifier string.
    ///
    /// `provider_url` should be a [`crate::providers::Provider`] constant
    /// (e.g. `Provider::EARTH_SEARCH_V1`). Returns `""` if the
    /// (collection, provider) combination is not yet supported.
    ///
    pub fn id_for(self, provider_url: &str) -> &'static str {
        use crate::providers::Provider;
        match (self, provider_url) {
            (Collection::Sentinel2, p) if p == Provider::EARTH_SEARCH_V1 => "sentinel-2-l2a",
            (Collection::Sentinel2, p) if p == Provider::PLANETARY_COMPUTER => "sentinel-2-l2a",
            (Collection::Sentinel2, p) if p == Provider::DEA => "ga_s2am_ard_3",
            (Collection::Landsat8, p) if p == Provider::EARTH_SEARCH_V1 => "landsat-c2-l2",
            (Collection::Landsat8, p) if p == Provider::PLANETARY_COMPUTER => "landsat-c2-l2",
            (Collection::Landsat8, p) if p == Provider::USGS_LANDSAT_LOOK => "landsat-c2l2-sr",
            (Collection::Landsat7, p) if p == Provider::EARTH_SEARCH_V1 => "landsat-c2-l2",
            (Collection::Landsat7, p) if p == Provider::PLANETARY_COMPUTER => "landsat-c2-l2",
            (Collection::Landsat5, p) if p == Provider::EARTH_SEARCH_V1 => "landsat-c2-l2",
            (Collection::Landsat5, p) if p == Provider::PLANETARY_COMPUTER => "landsat-c2-l2",
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;

    /// **RED T2.1/A1**: Sentinel2 → "sentinel-2-l2a" on Earth Search v1.
    #[test]
    fn sentinel2_resolves_on_earth_search_v1() {
        assert_eq!(
            Collection::Sentinel2.id_for(Provider::EARTH_SEARCH_V1),
            "sentinel-2-l2a"
        );
    }

    /// **RED T2.1/A2**: Sentinel2 → "sentinel-2-l2a" on Planetary Computer.
    #[test]
    fn sentinel2_resolves_on_planetary_computer() {
        assert_eq!(
            Collection::Sentinel2.id_for(Provider::PLANETARY_COMPUTER),
            "sentinel-2-l2a"
        );
    }

    /// **RED T2.1/A3**: Sentinel2 → "ga_s2am_ard_3" on DEA.
    #[test]
    fn sentinel2_resolves_on_dea() {
        assert_eq!(
            Collection::Sentinel2.id_for(Provider::DEA),
            "ga_s2am_ard_3"
        );
    }

    /// **RED T2.1/A4**: Landsat8 → "landsat-c2-l2" on Earth Search + PC.
    #[test]
    fn landsat8_resolves_on_earth_search_and_pc() {
        assert_eq!(
            Collection::Landsat8.id_for(Provider::EARTH_SEARCH_V1),
            "landsat-c2-l2"
        );
        assert_eq!(
            Collection::Landsat8.id_for(Provider::PLANETARY_COMPUTER),
            "landsat-c2-l2"
        );
    }
}
