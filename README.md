# orbit-etl — Phase-1 MVP

> File → Polars → SQLite ETL with gRPC + CLI clients.
> Foundation crate for the full `orbit-rs` platform (ETL + LLM agent + satellite/geo).
>
> 🛰️ **openEO**: `apps/orbit-openeo` is a **reference, NOT certified** openEO 1.3.0 backend — see the scope contract at [`apps/orbit-openeo/BACKEND-SCOPE.md`](apps/orbit-openeo/BACKEND-SCOPE.md). Strategic basis: [`13-geo-satellite/04-openeo-strategic-analysis.md`](../../13-geo-satellite/04-openeo-strategic-analysis.md) §4.5 (Approach D).
> 📚 **Docs**: see [`docs/README.md`](docs/README.md) for the layout map (plans, parity, perf, archive).

---

## What this MVP demonstrates

| Capability | Where |
|---|---|
| Workspace layout (5 crates) | `Cargo.toml` |
| Edition 2024 with `resolver = "3"` | `Cargo.toml` |
| Pipeline trait + engine | `crates/orbit-etl/src/engine.rs` |
| Polars LazyFrame reader (CSV/Parquet/JSON) | `engine::read_source` |
| Polars SQL transform | `engine::execute` |
| SQLite WAL mode + dedupe | `engine::ensure_table` / `insert_chunk` |
| Job state persistence | `orbit_jobs` table |
| Streaming progress events | `tokio::sync::mpsc` + `tokio-stream` |
| Tonic gRPC service | `apps/orbit-server/src/service.rs` |
| Clap CLI w/ progress bar | `apps/orbit-cli/src/main.rs` |
| End-to-end integration test | `tests/end_to_end.rs` |
| Graceful shutdown | `apps/orbit-server/src/main.rs` |

---

## Build

```bash
cd /Users/macbookpro/Rust/mvp/orbit-etl

# Sanity check
cargo check --workspace

# Build everything
cargo build --workspace --release

# Optional: install CLI on $PATH
cargo install --path apps/orbit-cli
cargo install --path apps/orbit-server
```

> Build deps: `protoc` (Tonic build) is needed only if you regenerate proto code; `tonic-build` bundles a prost protoc by default, so no system install is required.

---

## Run

### 1. Start the server

```bash
mkdir -p data
cargo run --release -p orbit-server
# Listening on 127.0.0.1:9876
```

Or with custom DB / bind:
```bash
ORBIT_DB="sqlite://./data/orbit.db?mode=rwc" \
ORBIT_BIND=127.0.0.1:9876 \
cargo run --release -p orbit-server
```

### 2. Run an ETL pipeline (in another terminal)

```bash
# Ingest the sample CSV into table `events`
cargo run --release -p orbit-cli -- \
    etl run \
    --input examples/sample.csv \
    --table events

# With a SQL transform: only US records, deduped by event_id
cargo run --release -p orbit-cli -- \
    etl run \
    --input examples/sample.csv \
    --table events_us \
    --sql "SELECT user_id, event_id, country, amount, timestamp FROM input WHERE country = 'US'" \
    --dedupe event_id
```

You'll see a streaming progress bar:
```
⠹ read=10 written=10
✓ completed read=10 written=10 (b7a82...)
```

### 3. Inspect jobs

```bash
cargo run --release -p orbit-cli -- etl list
cargo run --release -p orbit-cli -- etl status <JOB_ID>
```

### 4. Query the data

```bash
sqlite3 ./data/orbit.db 'SELECT country, COUNT(*) FROM events GROUP BY country;'
sqlite3 ./data/orbit.db 'SELECT * FROM events_us;'
```

---

## Test

```bash
cargo nextest run --workspace      # if you have cargo-nextest installed
# or:
cargo test --workspace
```

The integration test in `tests/end_to_end.rs` exercises the engine directly (no gRPC layer), creates a temp SQLite DB, runs the sample CSV through the SQL transform + dedupe, and verifies the row count.

> The test depends on `tempfile`. Add to `dev-dependencies` if you haven't:
> ```toml
> [dev-dependencies]
> tempfile = "3"
> tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
> tokio-stream = { workspace = true }
> sqlx = { workspace = true }
> ```
> (already configured in `crates/orbit-etl/Cargo.toml` via `[dev-dependencies]` — add if missing.)

---

## Architecture (this MVP)

```
┌─────────────────────────┐   gRPC      ┌─────────────────────────────┐
│       orbit-cli         │ ──────────▶ │       orbit-server          │
│  (clap + indicatif)     │             │  (tonic + EtlService)       │
└─────────────────────────┘             └────────────┬────────────────┘
                                                      │
                                          ┌───────────▼───────────┐
                                          │       orbit-etl       │
                                          │ ┌───────────────────┐ │
                                          │ │ Polars Lazy reads │ │
                                          │ │ Polars SQL xform  │ │
                                          │ │ SQLite WAL writes │ │
                                          │ │ Job state mgmt    │ │
                                          │ └───────────────────┘ │
                                          └───────────┬───────────┘
                                                      │
                                                ┌─────▼─────┐
                                                │  SQLite   │
                                                │ (./data)  │
                                                └───────────┘

shared: orbit-core (errors, JobId, JobStatus, JobState)
shared: orbit-proto (proto-generated gRPC types)
```

---

## What's next (Phases 2-7)

Phase 2 — add object_store source/sink (read from S3, write Parquet) — see [`11-framework-design/01-storage-workflow.md`](../../11-framework-design/01-storage-workflow.md)

Phase 3 — LLM agent crate (`orbit-agent`) — see [`11-framework-design/02-domain-blueprints.md` Domain B](../../11-framework-design/02-domain-blueprints.md#domain-b--llm-agent-platform)

Phase 4 — Satellite/geo crate (`orbit-geo`) — see [`13-geo-satellite/02-stac-sentinel2-2026.md`](../../13-geo-satellite/02-stac-sentinel2-2026.md) (STAC + Sentinel-2 deep research) and [`13-geo-satellite/01-geospatial-reference.md`](../../13-geo-satellite/01-geospatial-reference.md) (broader geo stack)

Phase 5 — Multi-node (NATS, chitchat, openraft) — see [`12-distributed-systems/01-distributed-reference.md`](../../12-distributed-systems/01-distributed-reference.md)

Phase 6 — REST/WS + Python wheel — see [`04-web-frameworks/01-web-frameworks-reference.md`](../../04-web-frameworks/01-web-frameworks-reference.md)

Phase 7 — Plugins + resilience — see [`14-resilience-plugins/01-plugins-resilience.md`](../../14-resilience-plugins/01-plugins-resilience.md)

---

## License

MIT OR Apache-2.0 (pick when you publish).
