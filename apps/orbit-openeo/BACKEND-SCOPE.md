# `orbit-openeo` Backend Scope Contract

> **Status**: Active. Created 2026-05-22.
> **Audience**: Contributors adding routes, process nodes, auth paths, or any HTTP-facing behaviour to `apps/orbit-openeo`.
> **Authority**: [`13-geo-satellite/04-openeo-strategic-analysis.md`](../../../../13-geo-satellite/04-openeo-strategic-analysis.md) §4.5 (Approach D — client adapter + reference backend). This file is the operational instance of §4.5.3.

**Read this before opening a PR.** Any PR violating the WILL NOT list below requires a separate change to `04-openeo-strategic-analysis.md` § 4.5.3 *first*, with explicit re-opening of the Approach-D decision.

---

## 1. Posture

`apps/orbit-openeo` is a **reference openEO 1.3.0 backend** — small, opinionated, intentionally non-certified. It exists so that the existing openEO Python/R/JS ecosystem can drive `orbit-geo` compute remotely without us becoming Approach B (full conformance backend) by accident.

- **Spec pin**: openEO 1.3.0. The shipped `spec/openapi.json` is the source of truth at request time (validated via `jsonschema`).
- **Conformance**: Certification not pursued. [`openeo-api-validator`](https://github.com/Open-EO/openeo-api-validator) will not be run in CI and certification will not be claimed publicly. (Individual processes *do* follow the 1.3.0 spec for params/semantics — "spec-faithful processes" is not the same as "certified API".)
- **Tenancy**: Single-tenant reference deployment. No multi-tenant features, no per-user billing.
- **Public claim**: README, `/.well-known/openeo`, and the project landing page must label this as a **"reference backend, not certified"** implementation.

---

## 2. The Bounded Process Set

The local `GeoExecutor` (`apps/orbit-openeo/src/geo_executor/`) implements an openEO 1.3.0 process subset. The **authoritative list** is `geo_executor/registry.rs::register_defaults` — **67 processes as of 2026-05-25** (up from 8 at 0.1.0). Representative cube-level nodes:

| Process | openEO spec ID | Status |
|---|---|---|
| `load_collection` | `load_collection` | ✅ (DN→reflectance scaling from STAC `raster:bands`) |
| `filter_temporal` / `filter_spatial` / `filter_bands` | same | ✅ |
| `mask` / `mask_scl_dilation` | `mask` (+ convenience) | ✅ (per-band SCL resample for mixed-resolution) |
| `reduce_dimension` | `reduce_dimension` | ✅ 10 reducers (mean/min/max/sum/median/count/first/last/sd/variance) + **arbitrary callbacks**, over `t` **and** `bands` axes |
| `merge_cubes` | `merge_cubes` | ✅ Case 1 (band-axis join) + Case 2 (`overlap_resolver`) + Case 3 (spatial mosaic) |
| `apply` | `apply` | ✅ per-pixel sub-graph (arithmetic, comparison, boolean, `clip`, `power`/`log`/`exp`, `linear_scale_range`) over **all** bands |
| `rename_labels` / `add_dimension` / `drop_dimension` | same | ✅ |
| `save_result` | `save_result` | ✅ (formats: GTiff, PNG; Zarr/NetCDF not implemented) |
| `ndvi` (convenience) | `ndvi` | ✅ |

Plus **31 scalar math/logic** processes (`absolute`/`sqrt`/`power`/`normalized_difference`/`clip`/comparison/boolean/trig/…) and **9 array** processes (`array_element`/`array_create`/`sort`/`order`/…). See `registry.rs::register_defaults` for the exact set.

The `LocalExecutor` additionally accepts arithmetic-only graphs (no `load_collection`) for unit testing.

Unknown processes reject via the `ProcessRegistry` with an `UnknownProcess` error (plus a Levenshtein "did you mean" hint). The earlier **G1.6** plan (typed `ProcessNode` enum as the sole dispatcher) was **superseded (2026-05-25)** by `ProcessRegistry` + `ProcessHandler` trait dispatch.

---

## 3. MAY (the backend is allowed to do this)

- Accept openEO process graphs over HTTP POST and execute the process subset in §2.
- Persist jobs to SQLite via `SqliteJobStore` and report status via REST + WebSocket subscription.
- Sign Planetary Computer asset URLs on the user's behalf (`crates/orbit-geo/src/providers.rs`).
- Return `save_result` outputs as GeoTIFF / PNG (STAC Item emission not yet implemented).
- Authenticate via OIDC device-flow, HTTP Basic, or Bearer token (constant-time-compared).
- Enforce a 128 MiB request body limit (`main.rs:81-87`).
- Refuse to start on a non-loopback address without an auth token (security default).
- Run JSON-Schema validation against `spec/openapi.json` at request time.
- Honour `DELETE /jobs/{id}` by cancelling the running `ExecutionContext` (after **G1.5** lands).

---

## 4. WILL NOT (the backend will not do this without reopening Approach D)

This list is exhaustive for change-control purposes. **Each line is a hard stop for PR review.**

- ❌ Will not pursue or claim openEO API conformance certification.
- ❌ Will not implement openEO processes outside §2 without owner sign-off. (The §2 set was expanded **8 → 67 on 2026-05-25**, strictly against the openEO 1.3.0 spec; the former `docs/plans` change-control tracker was retired with the `docs/` tree, so additions are now governed by §2 + this contract directly.)
- ❌ Will not track openEO spec revisions automatically. Spec is pinned to 1.3.0; future revisions are opt-in only.
- ❌ Will not add user/billing/quota systems beyond per-user job ownership.
- ❌ Will not host other tenants' compute (no multi-tenant deployment).
- ❌ Will not add federated identity (single-IdP only).
- ❌ Will not add documentation in non-English languages.
- ❌ Will not add side-channel APIs that compete with the openEO surface (e.g. a parallel REST schema for the same operations).
- ❌ Will not embed third-party processing engines (Geotrellis, openEO-GeoPySpark, etc.) — orbit-geo is the only compute backend.
- ❌ Will not relax the loopback-or-auth boot guard (`main.rs:81-87`).
- ❌ Will not weaken the `subtle::ConstantTimeEq` bearer check.
- ⚠️ ~~Will not introduce string-dispatch for process nodes once **G1.6** has typed `ProcessNode` enum as the only dispatcher.~~ **Superseded 2026-05-25**: the backend deliberately dispatches via `ProcessRegistry` (string-keyed `ProcessHandler` lookup with a did-you-mean hint). G1.6's typed-enum-only goal was not adopted.

---

## 5. Change-Control Process

To change anything in §4 WILL NOT:

1. Open a PR against [`13-geo-satellite/04-openeo-strategic-analysis.md`](../../../../13-geo-satellite/04-openeo-strategic-analysis.md) §4.5.3 explaining the new scope and revised maintenance commitment from §4.5.4.
2. Get explicit owner approval (`@NU` — solo-dev project, no CODEOWNERS).
3. Land the strategic-doc change **before** the implementation PR.
4. The implementation PR must link back to the §4.5.3 commit by SHA.

To add a process to §2 (without touching §4):

1. Add a handler in `geo_executor/registry.rs::register_defaults` + an `eval_*` impl (or a `*_handler!` macro).
2. Include a public reference for the algorithm (no upstream raster engine source).
3. Ship a unit test (TDD: RED → GREEN) before registering the handler.
4. Update §2 above (and `register_defaults`) in the same PR.

---

## 6. Enforcement Gates

The contract is enforced by these CI / review hooks:

| Gate | Enforces | Status |
|---|---|---|
| `jsonschema` validation against `spec/openapi.json` at request time | Spec pin (1.3.0) | ✅ active |
| `subtle::ConstantTimeEq` bearer check | Timing-safe auth | ✅ active |
| Loopback-or-auth boot guard | Single-tenant default | ✅ active |
| `ProcessRegistry` handler dispatch + did-you-mean | §2 bounded set | ✅ active (superseded G1.6) |
| `wiremock-rs` client test suite | Client contract | ⏳ G6.1 |
| Cancellation round-trip test | DELETE /jobs/{id} works | ⏳ G6.2 |
| Hard clippy gate (no `continue-on-error`) | Code quality | ⏳ G7.1 |
| `cargo semver-checks` on releases | API stability | ⏳ G7.2 |
| CycloneDX SBOM on releases | Supply chain | ⏳ G7.3 |
| Parity harness vs openEO Python (full 6-node graph) | "Byte-identical" claim | ⏳ G3.5 |

Tasks marked ⏳ are tracked socially via this document + PR review (the former `docs/plans` tracker was retired with the `docs/` tree on 2026-05-25).

---

## 7. Maintenance Budget

Per `04-openeo-strategic-analysis.md` §4.5.4:

- Estimated marginal cost vs Approach C: **+1 dev-day per week** of active development.
- Cost is dominated by integration tests against the backend HTTP surface and JSON-Schema drift.
- Security patches for `axum`, `tonic`, `reqwest`, `sqlx`, `jsonschema` are now in the runtime surface — they are no longer optional.
- Rust 1.88+ MSRV (`workspace.package.rust-version = "1.88"`) must be maintained.
- openEO 1.3.0 fixtures must remain green; spec-pin discipline is non-negotiable.

If the marginal cost trends above 2 dev-days/week sustained for 4 weeks, **the backend should be paused** and Approach C (client-only) reconsidered. This is not a hypothetical — it's the bus-factor mitigation §4.5.2 promised.

---

## 8. Sunset Path

If openEO 1.3.0 is sunset by the upstream community or this project loses dev capacity:

1. Mark `apps/orbit-openeo` as `#[deprecated]` at the next minor release.
2. Update README + `/.well-known/openeo` response with the sunset date and migration guidance.
3. Keep the binary buildable for 2 minor versions to give downstream users migration time.
4. Remove from the workspace on the third minor release.
5. The `crates/orbit-geo/src/openeo.rs` client adapter remains regardless — sunsetting the backend does not affect Approach C client functionality.

---

## 9. Cross-References

- Strategic basis: [`13-geo-satellite/04-openeo-strategic-analysis.md`](../../../../13-geo-satellite/04-openeo-strategic-analysis.md) §4.5
- Live runbook + deferred work: [`CLAUDE.md`](../../CLAUDE.md) (build/run/perf; §9 STAC-client consolidation)
- Client-adapter retrospective: [`13-geo-satellite/04-openeo-strategic-analysis.md`](../../../../13-geo-satellite/04-openeo-strategic-analysis.md) §8
- openEO spec: <https://openeo.org/documentation/1.0/developers/api/reference.html> (pinned to 1.3.0)
- License hygiene: [`NOTICE.md`](../../NOTICE.md), [`THIRD_PARTY.md`](../../THIRD_PARTY.md)
