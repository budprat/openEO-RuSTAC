# orbit-openeo

> **Reference openEO 1.3.0 backend — NOT certified.** See [`BACKEND-SCOPE.md`](./BACKEND-SCOPE.md) before opening a PR.

Axum-based HTTP server exposing `orbit-geo` compute via the openEO REST API. Single-tenant, opinionated, intentionally non-conformant.

## Read first

| Document | Why |
|---|---|
| [`BACKEND-SCOPE.md`](./BACKEND-SCOPE.md) | **MAY / WILL NOT contract.** Any PR adding a route, process node, or auth path must satisfy §4 (WILL NOT) without exception. |
| [`../../CLAUDE.md`](../../CLAUDE.md) | Live build / run / perf runbook + §9 deferred work (e.g. STAC-client consolidation). |
| [`../../../../13-geo-satellite/04-openeo-strategic-analysis.md`](../../../../13-geo-satellite/04-openeo-strategic-analysis.md) §4.5 | Strategic basis for the reference-backend posture (Approach D). |

## Running

```sh
cargo run -p orbit-openeo -- --bind 127.0.0.1:9080
```

Non-loopback addresses **require** an auth token (`--auth-token` / `ORBIT_OPENEO_AUTH_TOKEN`; see `main.rs`).

## Surface

- openEO 1.3.0 endpoints under `routes/`
- Spec-source-of-truth: [`spec/openapi.json`](./spec/openapi.json) — JSON-Schema-validated at request time
- Process executor: `ProcessRegistry` handler dispatch (`src/geo_executor/registry.rs`) over `eo-process` + `orbit-geo` — **67 processes** (see `registry.rs::register_defaults`)
- Job persistence: `SqliteJobStore` (+ orphan recovery on startup; `ORBIT_JOB_TIMEOUT_SECS`)

## What this backend will NOT do

Per [`BACKEND-SCOPE.md`](./BACKEND-SCOPE.md) §4:

- ❌ Pursue openEO conformance certification
- ❌ Implement processes outside the bounded set in `BACKEND-SCOPE.md` §2
- ❌ Track openEO spec revisions automatically (pinned to 1.3.0)
- ❌ Host multi-tenant compute

Violating these requires reopening Approach D in the strategic doc *first*.
