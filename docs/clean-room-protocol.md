# Clean-Room Protocol

Every contributor to `mvp/orbit-etl` MUST read and follow this document.

## Why

`mvp/orbit-etl` is licensed **MIT OR Apache-2.0**. The sibling `eors_workspace`
(JRSRP's `eorst` + `rss_core`) is licensed **LGPL-3.0**.

`orbit-geo` (and by extension `eo-*`) intentionally mirrors the *public-API
shape* of `eorst` so users can swap engines. The implementation, however, is
**clean-room**: written from observation of the public surface only, never
from the `eors_workspace` source code.

Reading LGPL source while writing MIT code creates a derivative work and
contaminates the licence. This protocol defines what's allowed.

## The four-quadrant rule

|                            | Reading `eors_workspace` source | Reading public references (specs, papers, Apache-2.0 forks) |
|---|---|---|
| **Writing behavioural specs / API descriptions** | ✅ allowed | ✅ allowed |
| **Writing tests** | ❌ forbidden | ✅ required |
| **Writing implementation code** | ❌ forbidden | ✅ required |
| **Reviewing a PR** | ❌ forbidden | ✅ required |

Behavioural specs describe *what* a function does observably (inputs, outputs,
edge cases). Function signatures and observable behaviour are not copyrightable
in the United States (Oracle v. Google). **Implementation is.**

## The 24-hour wash period

If you have just read `eors_workspace` source on a topic, wait **24 hours**
before writing orbit code on the same topic. The wash period:

- Reduces unconscious structural mimicry.
- Forces you to consult the spec / paper / public docs to refresh your
  understanding, which becomes the actual source of your implementation.

This is the solo-dev approximation of the two-team clean-room split used by
larger projects (Compaq, ReactOS).

## Allowed reference materials

These are *always* fair game:

- **Standards & specs**: STAC API 1.0.0, OpenEO API 1.2, OGC CQL2, GeoTIFF
  spec, Cloud-Optimized GeoTIFF spec, NetCDF CF Conventions, Zarr v3 spec.
- **Academic papers**: Zhu et al. (Fmask), Foga et al. (CFMask), Roy et al.
  (HLS), any author-published algorithm description.
- **Permissively-licensed reference implementations**: `rasterio`
  (BSD-3-Clause), `pystac-client` (Apache-2.0), `rio-tiler` (BSD-3-Clause),
  `sentinel-hub/sentinel2-cloud-detector` (Apache-2.0), `GDAL` (MIT/X).
- **Vendor documentation**: Planetary Computer, Earth Search, NASA EarthData,
  USGS Landsat C2 product guides.
- **Crate documentation** of any dependency in our Cargo.lock.

If a permissively-licensed source is ported, see
[`THIRD_PARTY.md`](../THIRD_PARTY.md) for the required attribution format.

## Forbidden materials

- `eors_workspace/libs/eorst/src/**`
- `eors_workspace/libs/rss_core/src/**`
- Any artefact derived from those trees (CSVs of public symbols, surface
  diffs in summary form, behavioural notes you took while reading) — **may**
  be used to write specs/tests, but **never** copy-pasted into orbit code.

## PR sign-off

Every PR description must include:

```
Clean-room: I did NOT consult eors_workspace source code while writing the
implementation in this PR. Reference materials used: <list>.
```

A PR that touches `crates/eo-*` without this line will not be merged.

## When you genuinely need to consult `eors_workspace`

Rare but possible: you suspect orbit produces different output than eors on
the same input and want to confirm it's a divergence, not a bug.

The allowed workflow is **black-box behavioural comparison**, not
**source-code consultation**:

1. Build `eors_workspace` separately.
2. Run both engines against a fixed input dataset.
3. Diff the outputs.
4. Document the divergence in `docs/parity/divergences.md` with rationale.

You may **not** open `eors_workspace/libs/eorst/src/**.rs` to "see how they
did it". If you do, the 24-hour wash applies and your next PR on that topic
must declare it.

## Attribution that *is* required

You are encouraged (and may be required by an upstream licence) to
credit the *idea*, the *algorithm*, or the *project that proved a design*:

- The README's "Anchor in orbit-rs" callouts already credit EORS as design
  inspiration. Keep doing this.
- New algorithm implementations cite the paper they implement.
- Ports from Apache-2.0 sources record the source repository, commit hash,
  and licence header in [`THIRD_PARTY.md`](../THIRD_PARTY.md).

Credit is good. Copying is bad. Specs and citations are how we have both.
