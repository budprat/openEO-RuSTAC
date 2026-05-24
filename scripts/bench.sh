#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo bench -p orbit-openeo --features geo-kernel --bench canonical_pipeline -- "$@"
