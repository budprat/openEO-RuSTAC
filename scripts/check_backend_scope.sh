#!/usr/bin/env bash
# CI gate: enforce that any PR adding routes to apps/orbit-openeo/src/routes/
# also updates apps/orbit-openeo/BACKEND-SCOPE.md in the same diff.
#
# Reasoning: BACKEND-SCOPE.md §2 lists the bounded process set + endpoint
# surface; new HTTP routes that don't appear in §2 silently expand the
# attack surface without re-opening Approach D (see strategic doc §4.5.3).
#
# Heuristic (not perfect — false-positive friendly, will catch most slips):
#   1. Find new `.route(...)` calls in apps/orbit-openeo/src/routes/**.rs
#   2. If any were added, require BACKEND-SCOPE.md to be in the same diff.
#   3. Failure mode: maintainer adds `[scope-ok: <reason>]` to commit message
#      to override (audited in PR review).

set -euo pipefail

BASE="${1:-origin/main}"
HEAD="${2:-HEAD}"

ROUTE_GLOB='apps/orbit-openeo/src/routes/'
SCOPE_FILE='apps/orbit-openeo/BACKEND-SCOPE.md'

# Count added .route( calls under routes/
ADDED_ROUTES=$(git diff "$BASE...$HEAD" -- "$ROUTE_GLOB" \
  | grep -E '^\+.*\.route\(' \
  | grep -v '^\+\+\+' \
  | wc -l | tr -d ' ')

if [[ "$ADDED_ROUTES" == "0" ]]; then
  echo "✓ no new .route() additions in $ROUTE_GLOB — scope check skipped"
  exit 0
fi

# A new route was added; require BACKEND-SCOPE.md to also be touched
if git diff --name-only "$BASE...$HEAD" | grep -qx "$SCOPE_FILE"; then
  echo "✓ $ADDED_ROUTES route addition(s) detected; $SCOPE_FILE was also updated"
  exit 0
fi

# Escape hatch — commit message contains [scope-ok: ...]
if git log "$BASE..$HEAD" --format=%B | grep -qE '\[scope-ok:'; then
  echo "⚠ $ADDED_ROUTES route addition(s) without $SCOPE_FILE update, but [scope-ok: ...] override found in commit message"
  exit 0
fi

cat <<EOF >&2
✗ FAIL — $ADDED_ROUTES new route(s) added under $ROUTE_GLOB
         but $SCOPE_FILE was not updated.

         Per BACKEND-SCOPE.md §4 and strategic-analysis §4.5.3, every new
         HTTP route must either appear in §2 (bounded process set) or be
         declared in §3 (MAY) of $SCOPE_FILE.

         To fix:
           1. Update $SCOPE_FILE §2 or §3 to cover the new route(s), OR
           2. If the addition is non-substantive (rename, refactor, test-only),
              add "[scope-ok: <one-line reason>]" to the commit message.
EOF
exit 1
