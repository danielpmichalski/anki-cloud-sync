#!/usr/bin/env bash
# Smoke test suite for the internal sidecar API.
# Covers: auth, decks CRUD, notes CRUD, bulk create, pagination on all 3 list endpoints.
#
# Usage:
#   ./scripts/smoke-test.sh          # builds then tests
#   ./scripts/smoke-test.sh --no-build   # skip build (use existing binary)
set -euo pipefail

# ---- Config ---------------------------------------------------------------
TOKEN="smoke-test-token"
EMAIL="smoke@example.com"
PASS="testpass"
SYNC_PORT=18080
INTERNAL_PORT=18081
BASE_URL="http://127.0.0.1:${INTERNAL_PORT}/internal/v1"
BINARY="./target/debug/anki-sync-server"
SYNC_BASE=$(mktemp -d)
SERVER_PID=""

# ---- Helpers ---------------------------------------------------------------
PASS_COUNT=0
FAIL_COUNT=0

pass() { echo "  ✓ $1"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { echo "  ✗ $1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$actual" == "$expected" ]]; then pass "$label"
    else fail "$label — expected '$expected', got '$actual'"; fi
}

assert_contains() {
    local label="$1" needle="$2" haystack="$3"
    if echo "$haystack" | grep -q "$needle"; then pass "$label"
    else fail "$label — '$needle' not in: $haystack"; fi
}

assert_http() {
    local label="$1" expected_code="$2" actual_code="$3" body="$4"
    if [[ "$actual_code" == "$expected_code" ]]; then pass "$label (HTTP $actual_code)"
    else fail "$label — expected HTTP $expected_code, got $actual_code | body: $body"; fi
}

# curl wrapper: sets headers, returns "STATUS_CODE BODY" separated by newline
api() {
    local method="$1" path="$2"; shift 2
    local extra_args=("$@")
    curl -s -o /tmp/smoke_body -w "%{http_code}" \
        -X "$method" \
        -H "X-Internal-Token: ${TOKEN}" \
        -H "X-User-Email: ${EMAIL}" \
        -H "Content-Type: application/json" \
        ${extra_args[@]+"${extra_args[@]}"} \
        "${BASE_URL}${path}"
}

api_with_body() {
    local method="$1" path="$2" body="$3"
    api "$method" "$path" -d "$body"
}

# ---- Lifecycle -------------------------------------------------------------
cleanup() {
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$SYNC_BASE"
}
trap cleanup EXIT

start_server() {
    SYNC_USER1="${EMAIL}:${PASS}" \
    SYNC_BASE="$SYNC_BASE" \
    SYNC_PORT="$SYNC_PORT" \
    SYNC_INTERNAL_PORT="$INTERNAL_PORT" \
    SYNC_INTERNAL_TOKEN="$TOKEN" \
        "$BINARY" &>/tmp/smoke_server.log &
    SERVER_PID=$!

    # Wait up to 5s for internal port to accept connections
    local i=0
    while ! nc -z 127.0.0.1 "$INTERNAL_PORT" 2>/dev/null; do
        sleep 0.2
        i=$((i + 1))
        if [[ $i -ge 25 ]]; then
            echo "ERROR: server did not start. Log:"
            cat /tmp/smoke_server.log
            exit 1
        fi
    done
}

# ---- Build -----------------------------------------------------------------
if [[ "${1:-}" != "--no-build" ]]; then
    echo "==> Building..."
    cargo build --bin anki-sync-server 2>&1 | tail -3
fi

echo ""
echo "==> Starting server (port ${SYNC_PORT}, internal ${INTERNAL_PORT})..."
start_server
echo "    server PID $SERVER_PID — ready"
echo ""

# =========================================================================
# Section 1: Auth
# =========================================================================
echo "-- Auth --"

code=$(curl -s -o /tmp/smoke_body -w "%{http_code}" \
    -H "X-Internal-Token: WRONG" -H "X-User-Email: ${EMAIL}" \
    "${BASE_URL}/decks")
assert_http "wrong token → 401" "401" "$code" "$(cat /tmp/smoke_body)"

code=$(curl -s -o /tmp/smoke_body -w "%{http_code}" \
    -H "X-User-Email: ${EMAIL}" \
    "${BASE_URL}/decks")
assert_http "missing token → 401" "401" "$code" "$(cat /tmp/smoke_body)"

# =========================================================================
# Section 2: Decks — list (empty)
# =========================================================================
echo ""
echo "-- Decks: list --"

code=$(api GET /decks)
body=$(cat /tmp/smoke_body)
assert_http "list decks (empty collection)" "200" "$code" "$body"
# A fresh collection always has a "Default" deck
assert_contains "response has 'decks' key" '"decks"' "$body"
assert_contains "nextCursor key present" '"nextCursor"' "$body"

# Parse Default deck id for later use
DEFAULT_DECK_ID=$(echo "$body" | python3 -c "import sys,json; decks=json.load(sys.stdin)['decks']; print(next(d['id'] for d in decks if d['name']=='Default'))" 2>/dev/null || echo "")
if [[ -z "$DEFAULT_DECK_ID" ]]; then
    # fallback: grab first id
    DEFAULT_DECK_ID=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin)['decks'][0]['id'])")
fi
echo "    Default deck id: $DEFAULT_DECK_ID"

# =========================================================================
# Section 3: Decks — create / get / delete
# =========================================================================
echo ""
echo "-- Decks: CRUD --"

code=$(api_with_body POST /decks '{"name":"TestDeck"}')
body=$(cat /tmp/smoke_body)
assert_http "create deck → 201" "201" "$code" "$body"
assert_contains "response has id" '"id"' "$body"
DECK_ID=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
echo "    created deck id: $DECK_ID"

code=$(api GET "/decks/${DECK_ID}")
body=$(cat /tmp/smoke_body)
assert_http "get deck → 200" "200" "$code" "$body"
assert_contains "deck name correct" "TestDeck" "$body"

code=$(api GET "/decks/9999999999")
body=$(cat /tmp/smoke_body)
assert_http "get unknown deck → 404" "404" "$code" "$body"

# =========================================================================
# Section 4: Notes — create single
# =========================================================================
echo ""
echo "-- Notes: create single --"

code=$(api_with_body POST "/decks/${DECK_ID}/notes" \
    '{"fields":{"Front":"Q1","Back":"A1"},"tags":["tag1"]}')
body=$(cat /tmp/smoke_body)
assert_http "create note → 201" "201" "$code" "$body"
NOTE_ID=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
echo "    created note id: $NOTE_ID"

# =========================================================================
# Section 5: Notes — bulk create
# =========================================================================
echo ""
echo "-- Notes: bulk create --"

code=$(api_with_body POST "/decks/${DECK_ID}/notes/bulk" \
    '{"notes":[
        {"fields":{"Front":"BQ1","Back":"BA1"},"tags":[]},
        {"fields":{"Front":"BQ2","Back":"BA2"},"tags":["bulk"]},
        {"fields":{"Front":"BQ3","Back":"BA3"},"tags":[]}
    ]}')
body=$(cat /tmp/smoke_body)
assert_http "bulk create 3 notes → 201" "201" "$code" "$body"
assert_contains "ids array returned" '"ids"' "$body"
BULK_COUNT=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['ids']))")
assert_eq "ids array length = 3" "3" "$BULK_COUNT"

# empty bulk
code=$(api_with_body POST "/decks/${DECK_ID}/notes/bulk" '{"notes":[]}')
body=$(cat /tmp/smoke_body)
assert_http "bulk create 0 notes → 201" "201" "$code" "$body"
EMPTY_COUNT=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['ids']))")
assert_eq "ids array length = 0" "0" "$EMPTY_COUNT"

# =========================================================================
# Section 6: Notes — get / update / delete
# =========================================================================
echo ""
echo "-- Notes: get / update / delete --"

code=$(api GET "/notes/${NOTE_ID}")
body=$(cat /tmp/smoke_body)
assert_http "get note → 200" "200" "$code" "$body"
assert_contains "note has fields" '"fields"' "$body"

code=$(api_with_body PUT "/notes/${NOTE_ID}" '{"fields":{"Front":"Q1-edited","Back":"A1"},"tags":["updated"]}')
body=$(cat /tmp/smoke_body)
assert_http "update note → 200" "200" "$code" "$body"

# verify edit persisted
code=$(api GET "/notes/${NOTE_ID}")
body=$(cat /tmp/smoke_body)
assert_contains "updated field visible" "Q1-edited" "$body"

code=$(api GET "/notes/9999999999")
assert_http "get unknown note → 404" "404" "$code" "$(cat /tmp/smoke_body)"

# =========================================================================
# Section 7: Pagination — list notes in deck
# =========================================================================
echo ""
echo "-- Pagination: list notes in deck (4 notes total) --"

# deck has 1 single + 3 bulk = 4 notes
code=$(api GET "/decks/${DECK_ID}/notes?limit=2")
body=$(cat /tmp/smoke_body)
assert_http "list notes limit=2 → 200" "200" "$code" "$body"
PAGE1_COUNT=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['notes']))")
assert_eq "page 1 has 2 notes" "2" "$PAGE1_COUNT"
NEXT_CURSOR=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin)['nextCursor'])")
assert_contains "nextCursor is set" '.' "$NEXT_CURSOR"   # non-empty
echo "    cursor after page 1: $NEXT_CURSOR"

code=$(api GET "/decks/${DECK_ID}/notes?limit=2&cursor=${NEXT_CURSOR}")
body=$(cat /tmp/smoke_body)
assert_http "list notes page 2 → 200" "200" "$code" "$body"
PAGE2_COUNT=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['notes']))")
assert_eq "page 2 has 2 notes" "2" "$PAGE2_COUNT"
CURSOR2=$(echo "$body" | python3 -c "import sys,json; v=json.load(sys.stdin)['nextCursor']; print(v if v else 'None')")
assert_eq "no more pages after page 2" "None" "$CURSOR2"

# =========================================================================
# Section 8: Pagination — search notes
# =========================================================================
echo ""
echo "-- Pagination: search notes --"

code=$(api GET "/notes/search?q=tag%3Abulk&limit=2")
body=$(cat /tmp/smoke_body)
assert_http "search tag:bulk limit=2 → 200" "200" "$code" "$body"
assert_contains "notes array present" '"notes"' "$body"
SEARCH_COUNT=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['notes']))")
assert_eq "search returns 1 bulk-tagged note" "1" "$SEARCH_COUNT"

# search all notes (deck filter) — 4 results, limit 3 → cursor present
code=$(api GET "/notes/search?q=deck%3ATestDeck&limit=3")
body=$(cat /tmp/smoke_body)
assert_http "search deck:TestDeck limit=3 → 200" "200" "$code" "$body"
SC2=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['notes']))")
assert_eq "search page 1 has 3 notes" "3" "$SC2"
SC2_CURSOR=$(echo "$body" | python3 -c "import sys,json; v=json.load(sys.stdin)['nextCursor']; print(v if v else 'None')")
assert_contains "search nextCursor set" '.' "$SC2_CURSOR"

code=$(api GET "/notes/search?q=deck%3ATestDeck&limit=3&cursor=${SC2_CURSOR}")
body=$(cat /tmp/smoke_body)
assert_http "search deck:TestDeck page 2 → 200" "200" "$code" "$body"
SC3=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['notes']))")
assert_eq "search page 2 has 1 note" "1" "$SC3"
SC3_CURSOR=$(echo "$body" | python3 -c "import sys,json; v=json.load(sys.stdin)['nextCursor']; print(v if v else 'None')")
assert_eq "search page 2 no more cursor" "None" "$SC3_CURSOR"

# =========================================================================
# Section 9: Pagination — list decks
# =========================================================================
echo ""
echo "-- Pagination: list decks --"

# Create a few more decks so we have >2
api_with_body POST /decks '{"name":"DeckA"}' >/dev/null
api_with_body POST /decks '{"name":"DeckB"}' >/dev/null
api_with_body POST /decks '{"name":"DeckC"}' >/dev/null

code=$(api GET "/decks?limit=2")
body=$(cat /tmp/smoke_body)
assert_http "list decks limit=2 → 200" "200" "$code" "$body"
D1=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['decks']))")
assert_eq "deck page 1 has 2 decks" "2" "$D1"
DC=$(echo "$body" | python3 -c "import sys,json; v=json.load(sys.stdin)['nextCursor']; print(v if v else 'None')")
assert_contains "deck nextCursor set" '.' "$DC"
echo "    deck cursor: $DC"

code=$(api GET "/decks?limit=2&cursor=${DC}")
body=$(cat /tmp/smoke_body)
assert_http "list decks page 2 → 200" "200" "$code" "$body"
D2=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['decks']))")
# we have Default + TestDeck + DeckA + DeckB + DeckC = 5, page1=2 → page2=2 or 3
[[ "$D2" -ge 1 ]] && pass "deck page 2 has ≥1 decks (got ${D2})" || fail "deck page 2 empty"

# =========================================================================
# Section 10: Invalid cursor — 400
# =========================================================================
echo ""
echo "-- Edge cases --"

code=$(api GET "/decks?cursor=notanumber")
body=$(cat /tmp/smoke_body)
assert_http "invalid cursor → 400" "400" "$code" "$body"

code=$(api GET "/decks/${DECK_ID}/notes?cursor=bad")
assert_http "invalid cursor on notes → 400" "400" "$code" "$(cat /tmp/smoke_body)"

code=$(api GET "/notes/search?q=*&cursor=bad")
assert_http "invalid cursor on search → 400" "400" "$code" "$(cat /tmp/smoke_body)"

# =========================================================================
# Section 11: Cleanup — delete note and deck
# =========================================================================
echo ""
echo "-- Cleanup / delete --"

code=$(api DELETE "/notes/${NOTE_ID}")
body=$(cat /tmp/smoke_body)
assert_http "delete note → 200" "200" "$code" "$body"

code=$(api DELETE "/decks/${DECK_ID}")
body=$(cat /tmp/smoke_body)
assert_http "delete deck → 200" "200" "$code" "$body"

# deleted deck should now 404
code=$(api GET "/decks/${DECK_ID}")
assert_http "get deleted deck → 404" "404" "$code" "$(cat /tmp/smoke_body)"

# =========================================================================
# Summary
# =========================================================================
echo ""
echo "================================================"
echo "Results: ${PASS_COUNT} passed, ${FAIL_COUNT} failed"
echo "================================================"
[[ $FAIL_COUNT -eq 0 ]]
