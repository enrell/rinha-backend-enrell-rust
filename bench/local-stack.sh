#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="${SOCK_DIR:-/tmp/rinha-socks}"
PORT="${PORT:-9998}"
mkdir -p "$SOCK"
rm -f "$SOCK"/*.sock

FD_PASS_PATH="$SOCK/api1.sock" "$ROOT/target/release/api" &
P1=$!
FD_PASS_PATH="$SOCK/api2.sock" "$ROOT/target/release/api" &
P2=$!
sleep 0.1
BACKEND_SOCKS="$SOCK/api1.sock,$SOCK/api2.sock" LISTEN_ADDR="127.0.0.1:$PORT" "$ROOT/target/release/lb" &
P3=$!
sleep 0.1
echo "stack pid lb=$P3 api1=$P1 api2=$P2 port=$PORT"

cleanup() {
  kill "$P3" "$P1" "$P2" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

curl -sf -m 2 "http://127.0.0.1:$PORT/ready" && echo " ready OK"
curl -sf -m 2 -X POST "http://127.0.0.1:$PORT/fraud-score" \
  -H 'Content-Type: application/json' \
  -d '{"id":"x","transaction":{"amount":1,"installments":1,"requested_at":"2026-03-11T20:23:35Z"},"customer":{"avg_amount":1,"tx_count_24h":1,"known_merchants":["MERC-001"]},"merchant":{"id":"MERC-001","mcc":"5912","avg_amount":1},"terminal":{"is_online":false,"card_present":true,"km_from_home":1}}'
echo " fraud OK"