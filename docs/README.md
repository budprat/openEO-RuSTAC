# `orbit-etl` Documentation Index

This directory holds the project's design, planning, and reference material. Below is the map.

## Layout

```
docs/
├── README.md                   ← you are here
├── clean-room-protocol.md      ← LGPL/MIT clean-room rules (cited by 01-MP §1.1)
│
├── plans/                      ← FORWARD WORK (where to look first)
│   ├── 01-maturity-and-parity.md   12-week roadmap (active)
│   └── 02-net-new-gaps.md          32 G-tasks across 7 tracks (active)
│
├── parity/                     ← MEASUREMENT vs the LGPL eors reference
│   ├── SURFACE.md                  mechanical surface diff (auto-regen by tools/api-diff)
│   ├── EORS_COMPONENT_COMPARISON.md  symbol-level audit (snapshot 2026-05-21)
│   └── MISSING_FEATURES.md         honest gap inventory (still load-bearing)
│
├── perf/                       ← PERFORMANCE
│   └── BENCHMARK_BASELINE.md       locked criterion numbers (CI gate compares against this)
│
└── archive/                    ← HISTORICAL execution records (Tier 0-5 complete)
    ├── 2026-05-21-parity-audit.md  was EORS_PARITY_AUDIT.md
    └── 2026-05-21-parity-plan.md   was EORS_PARITY_PLAN.md
```

## What to read for what

| If you want to… | Read |
|---|---|
| Understand the 12-week roadmap | [`plans/01-maturity-and-parity.md`](plans/01-maturity-and-parity.md) |
| Pick up an open production-gap task | [`plans/02-net-new-gaps.md`](plans/02-net-new-gaps.md) |
| Check what's left to implement vs eors | [`parity/MISSING_FEATURES.md`](parity/MISSING_FEATURES.md), [`parity/SURFACE.md`](parity/SURFACE.md) |
| Verify perf hasn't regressed | [`perf/BENCHMARK_BASELINE.md`](perf/BENCHMARK_BASELINE.md) |
| Understand the clean-room protocol before opening a PR | [`clean-room-protocol.md`](clean-room-protocol.md) |
| Understand why orbit is both an openEO client and reference backend | [`../../../13-geo-satellite/04-openeo-strategic-analysis.md`](../../../13-geo-satellite/04-openeo-strategic-analysis.md) §4.5 |
| Know what `apps/orbit-openeo` may and may not do | [`../apps/orbit-openeo/BACKEND-SCOPE.md`](../apps/orbit-openeo/BACKEND-SCOPE.md) |
| See historical (completed) execution records | [`archive/`](archive/) |

## Conventions

- **Plans** live in `plans/` and use `NN-slug.md` numbering. Each plan ends with a Definition of Done and a Risk Register.
- **Measurement artifacts** in `parity/` and `perf/` should be regeneratable. If a file is hand-authored once and never updated, it belongs in `archive/`.
- **Archived plans** keep their original content; do not edit. Add a follow-up plan if scope changes.
- **Cross-references use relative paths** so the repo can be browsed on disk or on the forge.
- **Dates in filenames** use `YYYY-MM-DD` format for sortability.

## Outside this directory

| File | Purpose |
|---|---|
| `../CHANGELOG.md` | Versioned release notes (Keep-a-Changelog format) |
| `../README.md` | Project landing page |
| `../NOTICE.md` | LGPL/MIT/Apache-2.0 acknowledgements |
| `../THIRD_PARTY.md` | Third-party attribution |
| `../deny.toml` | `cargo deny` policy (license + advisory allowlists) |
