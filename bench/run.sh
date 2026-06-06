#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MODE="${1:-perf}"
export BENCH_URL="${BENCH_URL:-http://127.0.0.1:9999}"
export BENCH_DURATION="${BENCH_DURATION:-15s}"
export BENCH_RATE="${BENCH_RATE:-900}"
export BENCH_TEST_DATA_HOST="${BENCH_TEST_DATA_HOST:-$ROOT/../rinha-de-backend-2026/test}"

SCRIPT="quick.js"
case "$MODE" in
  perf|quick) SCRIPT="quick.js" ;;
  mini)       SCRIPT="mini.js"; BENCH_DURATION="${BENCH_DURATION:-12s}"; BENCH_RATE="${BENCH_RATE:-70}" ;;
  smoke)      SCRIPT="smoke.js" ;;
  *)
    echo "uso: $0 [perf|mini|smoke]" >&2
    exit 1
    ;;
esac

echo ">> subindo API (1 CPU / 350 MB no compose da raiz)..."
if [[ "${NO_BUILD:-}" == "1" ]]; then
  docker compose up -d
else
  docker compose up -d --build
fi

echo ">> aguardando /ready..."
for _ in $(seq 1 50); do
  if curl -sf "$BENCH_URL/ready" >/dev/null 2>&1; then
    break
  fi
  sleep 0.2
done
curl -sf "$BENCH_URL/ready" >/dev/null || { echo "API não respondeu em $BENCH_URL"; exit 1; }

if [[ "$MODE" == "mini" && ! -f "$BENCH_TEST_DATA_HOST/test-data.json" ]]; then
  echo "test-data não encontrado em $BENCH_TEST_DATA_HOST/test-data.json" >&2
  echo "export BENCH_TEST_DATA_HOST=/caminho/para/pasta/test" >&2
  exit 1
fi

echo ">> benchmark ($MODE): k6 $SCRIPT  [cliente: 1 CPU / 512 MB]"
docker compose -f bench/docker-compose.yml run --rm k6 run "/bench/$SCRIPT"