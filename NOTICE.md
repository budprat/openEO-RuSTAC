# NOTICE

`mvp/orbit-etl` (the `orbit-rs` workspace) is licensed under
**MIT OR Apache-2.0**. See `LICENSE-MIT` and `LICENSE-APACHE` for full terms.

## Design inspiration

The public API surface of `orbit-geo` (and, going forward, the `eo-*`
crates) intentionally mirrors the shape of the JRSRP EORS Workspace
(`eorst` and `rss_core`, LGPL-3.0) so users can swap engines. The
implementation is clean-room — (capabilities identified from the public API shape, then reimplemented independently; no EORS source copied).

We credit EORS as the design inspiration; no code from EORS is included
in this project.

## Public references consulted

This project's design and implementation draw on the following publicly-
available, non-derivative references. None of these constitute a copyright
claim against orbit code; they are listed here as engineering credit and as
a record for license-compliance review.

### Standards
- **STAC API 1.0.0** — https://github.com/radiantearth/stac-api-spec
- **STAC Spec 1.0.0** — https://github.com/radiantearth/stac-spec
- **OpenEO API 1.2** — https://api.openeo.org
- **OGC CQL2** — https://docs.ogc.org/is/21-065r2/21-065r2.html
- **GeoTIFF Spec** — https://docs.ogc.org/is/19-008r4/19-008r4.html
- **Cloud-Optimized GeoTIFF Spec** — https://www.cogeo.org
- **Zarr v3** — https://zarr-specs.readthedocs.io/en/latest/v3/core/v3.0.html

### Academic papers
- Zhu, Wang & Woodcock (2015). *Improvement and expansion of the Fmask
  algorithm.* Remote Sensing of Environment, 159, 269–277.
- Foga et al. (2017). *Cloud detection algorithm comparison and validation
  for operational Landsat data products.* Remote Sensing of Environment,
  194, 379–390.
- Roy et al. (2010+). HLS / Landsat surface reflectance documentation.

### Vendor documentation
- Microsoft Planetary Computer — https://planetarycomputer.microsoft.com/docs
- Element84 Earth Search — https://earth-search.aws.element84.com/v1
- NASA EarthData — https://urs.earthdata.nasa.gov
- USGS Landsat C2 Product Guides — https://www.usgs.gov/landsat-missions

### Permissively-licensed software referenced
For ports / direct algorithm translations, see `THIRD_PARTY.md`.
