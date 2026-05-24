#!/usr/bin/env bash
# End-to-end test: submit NDVI mean-time process graph and observe lifecycle.
#
# Boots `orbit-openeo` on 127.0.0.1:9080 (loopback => no auth token needed),
# POSTs examples/ndvi_mean_time.json to /jobs, captures the OpenEO-Identifier,
# starts processing, polls until terminal state, then prints the result
# manifest.
#
# Usage:
#   ./examples/test_ndvi_e2e.sh                   # uses LocalExecutor (job → error: UnknownProcess "ndvi" — expected)
#   ORBIT_OPENEO_EXECUTOR=geo ./examples/test_ndvi_e2e.sh   # uses GeoExecutor (requires --features geo-kernel + GDAL)
#
# With LocalExecutor the job will surface a typed error during /results
# because LocalExecutor only understands {load_collection, save_result,
# add, subtract}. That is the *expected* terminal state for this script
# — it proves the API surface + topo-walker + runner lifecycle all work
# end-to-end on the openEO request shape.
set -euo pipefail

BIND="${ORBIT_OPENEO_BIND:-127.0.0.1:9080}"
EXECUTOR="${ORBIT_OPENEO_EXECUTOR:-local}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GRAPH="$HERE/ndvi_mean_time.json"

if [[ ! -f "$GRAPH" ]]; then
  echo "❌ missing graph: $GRAPH" >&2
  exit 1
fi

# Locate or build the binary. Direct execution avoids cargo's per-run
# linkage check which can take 30+ s on cold caches and blow the
# readiness window below.
WORKSPACE_ROOT="$(cd "$HERE/../../.." && pwd)"
BIN="$WORKSPACE_ROOT/target/debug/orbit-openeo"
if [[ "$EXECUTOR" == "geo" ]]; then
  BIN="$WORKSPACE_ROOT/target/debug/orbit-openeo-geo"
  if [[ ! -x "$BIN" ]]; then
    echo "🔨 building orbit-openeo with --features geo-kernel"
    cargo build -p orbit-openeo --features geo-kernel --quiet
    cp "$WORKSPACE_ROOT/target/debug/orbit-openeo" "$BIN"
  fi
fi
if [[ ! -x "$BIN" ]]; then
  echo "🔨 building orbit-openeo"
  cargo build -p orbit-openeo --quiet
fi

echo "🚀 starting $(basename "$BIN") (--executor $EXECUTOR, bind $BIND)"
"$BIN" --bind "$BIND" --executor "$EXECUTOR" >/tmp/orbit-openeo.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true; wait $SRV 2>/dev/null || true' EXIT

# Wait for /.well-known/openeo readiness.
READY=0
for i in $(seq 1 30); do
  if curl -sf "http://${BIND}/.well-known/openeo" >/dev/null 2>&1; then
    echo "✅ server up after ${i} tick(s)"
    READY=1
    break
  fi
  sleep 0.3
done
if [[ "$READY" != "1" ]]; then
  echo "❌ server failed to come up; log tail:"
  tail -40 /tmp/orbit-openeo.log
  exit 1
fi

echo
echo "📤 POST /jobs (NDVI mean-time graph)"
RESP="$(mktemp)"
HTTP=$(curl -sf -o "$RESP" -w "%{http_code}\n%header{openeo-identifier}\n%header{location}" \
    -X POST "http://${BIND}/jobs" \
    -H 'content-type: application/json' \
    --data-binary @"$GRAPH")
STATUS=$(echo "$HTTP" | sed -n 1p)
ID=$(echo "$HTTP" | sed -n 2p | tr -d '\r')
LOC=$(echo "$HTTP" | sed -n 3p | tr -d '\r')
echo "   status=$STATUS   id=$ID   location=$LOC"
cat "$RESP" | jq .
[[ "$STATUS" == "201" ]] || { echo "❌ expected 201"; exit 1; }
[[ -n "$ID" ]]            || { echo "❌ missing identifier"; exit 1; }

echo
echo "📥 GET /jobs/$ID (round-trip)"
curl -sf "http://${BIND}/jobs/$ID" | jq '{id, title, status, has_process_graph: (.process.process_graph | type)}'

echo
echo "▶️  POST /jobs/$ID/results (start processing)"
curl -sf -o /dev/null -w "   status=%{http_code}\n" -X POST "http://${BIND}/jobs/$ID/results"

echo
echo "⏳ polling for terminal state"
for i in $(seq 1 30); do
  REC="$(curl -sf "http://${BIND}/jobs/$ID")"
  ST=$(echo "$REC" | jq -r .status)
  PR=$(echo "$REC" | jq -r .progress)
  echo "   [$i] status=$ST progress=$PR"
  if [[ "$ST" == "finished" || "$ST" == "error" ]]; then break; fi
  sleep 0.5
done

echo
echo "📦 GET /jobs/$ID/results"
curl -s "http://${BIND}/jobs/$ID/results" | jq .

echo
echo "🧹 DELETE /jobs/$ID"
curl -sf -o /dev/null -w "   status=%{http_code}\n" -X DELETE "http://${BIND}/jobs/$ID"

echo
echo "✅ E2E roundtrip complete (executor=$EXECUTOR)"
