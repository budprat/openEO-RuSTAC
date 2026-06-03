# openEO 1.3.0 Endpoint / Database / Component Conformance Audit

> Scope: the HTTP surface, persistence, and supporting components of
> `apps/orbit-openeo`, audited against the bundled `spec/openapi.json`
> (**openEO API 1.3.0** — it defines the conformance class
> `https://api.openeo.org/1.3.0`). Date: 2026-06-03.
> Status: spec bugs fixed in the same change-set (✅ Fixed); deliberate
> BACKEND-SCOPE exclusions left as-is (⛔ Out of scope). Full lib suite green.

## 1. Capabilities & discovery

| Endpoint | Verdict | Notes |
|---|---|---|
| `GET /` capabilities | ✅ | Required fields present; **added `conformsTo`** (was missing). |
| `GET /.well-known/openeo` | ✅ | Version list. |
| `GET /conformance` | ✅ Fixed | Was advertised in capabilities `links` but **unmounted (404)**. Now returns `{conformsTo:[…]}`, equal to the capabilities array per spec. |

## 2. Data & processes

| Endpoint | Verdict | Notes |
|---|---|---|
| `GET /collections`, `/collections/{id}` | ✅ | STAC collections. |
| `GET /processes` | ✅ | Lists all 68 implemented processes (prior audit). |
| `GET /file_formats` | ✅ Fixed | **Added** the spec endpoint → `{input:{}, output:{GTiff,COG,PNG,JSON}}`. `/output_formats` (pre-1.0 name) kept as a back-compat alias. |
| `GET/POST /process_graphs` (UDPs) | ⚠️/⛔ | List key fixed `processes`→`process_graphs`. CRUD remains `501` — **UDP storage is out of scope** (BACKEND-SCOPE §4). |

## 3. Jobs (batch)

| Endpoint | Verdict | Notes |
|---|---|---|
| `GET/POST /jobs`, `GET/PATCH/DELETE /jobs/{id}` | ✅ | Job object has required fields; ids are now v4 UUIDs. Optional `costs`/`budget` omitted. |
| `GET/POST/DELETE /jobs/{id}/results` | ✅ Fixed | Manifest is now a **valid STAC Item** (`geometry`, `bbox`, `properties.datetime`, `self`/`canonical` links) — previously missing those required fields. |
| `GET /jobs/{id}/logs` | ✅ Fixed | **Added** → `{logs:[],links:[]}` (200 known job / 404 otherwise). Structured per-job log capture is future work. |
| `GET /jobs/{id}/estimate` | ⚠️ | Stub; not a full estimate object (`costs`/`duration`/`size`). |
| `POST /result` | ✅ | Synchronous compute. |

## 4. Users / auth / files / services

| Endpoint | Verdict | Notes |
|---|---|---|
| `GET /me` | ⚠️ | Returns `anonymous`; single-tenant reference posture (BACKEND-SCOPE). |
| `GET /credentials/basic` | ✅ | Implemented. |
| `POST /credentials/oidc/token` | ✅ | Device-flow token. The OIDC **provider-discovery** `GET /credentials/oidc` is not implemented; removed from the advertised endpoint list. |
| `GET /files/{user_id}` + `{path}` GET/PUT/DELETE | ✅ | User workspace files. |
| `services` / `service_types` / `udf_runtimes` | ⛔ | Stubs/empty — **secondary web services are out of scope** (BACKEND-SCOPE §4). |

## 5. Capabilities `endpoints` honesty

`discovery.rs::endpoint_list()` now mirrors the mounted routes: added
`/conformance`, `/file_formats`, `/jobs/{id}/logs`; corrected the OIDC token
path; dropped the unmounted `GET /credentials/oidc`.

## 6. Database

The SQLite `jobs` table (`id, user_id, title, description, process JSON,
status, progress, created, updated, assets JSON`) maps cleanly to the openEO
job object. Job ids are v4 UUIDs. No tables for UDPs/services (unimplemented
by design). ✅ Sound for the implemented surface.

## 7. Auth posture

Route security is derived from `spec/openapi.json` (`RouteSecurityMap`).
`/conformance` and `/file_formats` carry a public alternative (`security:
[{}, {Bearer}]`), so the auth layer serves them unauthenticated — matching the
spec. Capabilities, well-known, health, and metrics are public.

## 8. Version label

The backend correctly self-identifies as **openEO 1.3.0**: the bundled
`spec/openapi.json` is the 1.3.0 spec and declares
`https://api.openeo.org/1.3.0` as the general conformance class. (A prior note
speculating the label was ahead of the released spec was incorrect.)

## 9. Remaining gaps (deliberate — require BACKEND-SCOPE change-control)

- Secondary web services (`/services*`, `/service_types`) — not implemented.
- User-defined process graph storage (`/process_graphs` CRUD) — `501`.
- Full OIDC provider discovery; multi-tenant `/me`.
- `GET /jobs/{id}/estimate` is a stub; structured `GET /jobs/{id}/logs` capture
  is future work (the route now exists and is spec-shaped).
