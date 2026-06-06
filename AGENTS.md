# AGENTS.md — Rinha de Backend 2026 (Rust)

Rinha de Backend 2026 fraud detection challenge implemented in Rust with zero dependencies and raw syscalls.

## Project overview

Two binaries share a common fd-passing module:

| Binary | Path | Role |
|--------|------|------|
| `lb` | `src/bin/lb.rs` | TCP load balancer (port 9999), round-robin |
| `api` | `src/bin/api.rs` | HTTP API worker, serves fraud-score endpoint |
| `fdpass` | `src/fdpass.rs` | Shared Unix fd-passing via `sendmsg`/`recvmsg` |

**Architecture flow**: Client → `lb` (TCP :9999) —fd-pass over Unix socket→ `api` (serves HTTP directly on passed fd). No reverse-proxy HTTP parsing in the LB; it only round-robins raw TCP connections via fd handoff.

The API currently returns a **stub response** (`{"approved":true,"fraud_score":0}`). The actual fraud detection (14-dimension vectorization + k-NN vector search against `resources/references.json.gz`) is **not yet implemented**.

## Build & run

### Local build

```bash
cargo build --release --bin lb --bin api
```

### Docker

```bash
docker compose up -d --build    # starts lb + 2 api instances
```

### Benchmarks (requires a separate rinha-de-backend-2026 checkout for test data)

```bash
./bench/run.sh perf     # 900 req/s, 15s, single payload
./bench/run.sh mini     # 70 req/s, 12s, uses test-data.json
./bench/run.sh smoke    # 1 VU, 3 iterations, validates shape
```

Uses k6 with `constant-arrival-rate`. The `mini` mode needs `test-data.json` from the challenge repo at `../rinha-de-backend-2026/test/`.

## Runtime environment

- **`lb`**: env `LISTEN_ADDR` (default `0.0.0.0:9999`), `BACKEND_SOCKS` (comma-separated Unix socket paths)
- **`api`**: env `FD_PASS_PATH` (Unix socket path for receiving fds from lb)

Docker limits (total budget: 1 CPU, 350 MB):
- `lb`: 0.10 CPU, 16 MB
- `api1`/`api2`: 0.45 CPU each, 167 MB each

Unix sockets live on a tmpfs volume (`/sockets`).

## Code conventions & gotchas

### Zero dependencies — raw syscalls only

There is no `[dependencies]` in `Cargo.toml`. All I/O is done via `unsafe extern "C"` FFI directly to libc syscalls (`read`, `write`, `epoll_*`, `sendmsg`, `recvmsg`, `accept4`, `setsockopt`, `socket`, `bind`, `listen`, `close`). There is no tokio, no hyper, no serde — nothing. This is intentional for extreme performance under tight resource limits.

When adding functionality, **do not add dependencies** without careful consideration. Parse HTTP and JSON by hand.

### Shared module via `#[path]`

Both binaries include `src/fdpass.rs` via `#[path = "../fdpass.rs"] mod fdpass;` at the top. This is not a Cargo `[[lib]]` — it's a path-relative module inclusion. If you add shared code, follow this pattern.

### Rust edition 2024

The crate uses `edition = "2024"`. This is bleeding-edge and certain features (e.g. `unsafe` blocks inside `unsafe fn` body) work differently. Be aware of edition-specific semantics.

### Release profile

```toml
[profile.release]
codegen-units = 1
lto = "fat"
opt-level = 3
panic = "abort"
strip = true
```

Panic on abort means no unwinding — panics crash the process immediately.

### .cargo/config.toml

```toml
[build]
rustflags = ["-C", "target-cpu=native"]
```

Builds target the native CPU of the builder. This means binaries built on one machine may use instructions unavailable on another. The Docker build (on CI/runner) will use the runner's CPU features. The official test runner is an old Mac Mini 2014 (x86-64), so build with compatible target if cross-compiling.

### Epoll-based custom event loop

The `api` binary uses a single-threaded epoll event loop. Connections arrive via `TAG_CONTROL` events on the Unix socket. Each accepted TCP fd is wrapped in a `Client` struct with:
- A 16 KB buffer (`BUF_CAP`)
- Pending write tracking (`pending`, `pending_off`)

The `handle_client` function does buffered HTTP parsing, routing, and non-blocking writes with partial-write retry.

### HTTP parsing is manual and minimal

`request_total()` scans for `\r\n\r\n` then parses `Content-Length` via `content_length_fast()`. No full header parsing, no chunked encoding, no connection keep-alive beyond what TCP allows. The router matches by `starts_with` on method+path.

### Pending state

When `write` returns `WouldBlock`, the response pointer and offset are saved in `Client.pending`/`Client.pending_off`. On the next `EPOLLOUT`, `flush()` resumes the write. This is the non-blocking partial-write path.

### `handle_client` return semantics

Returns `true` if the client should stay registered in epoll, `false` if it should be dropped and closed. A client is kept alive as long as there's pending data to write or the buffer is still being filled.

## What needs to be built

The fraud detection pipeline (not yet implemented) requires:

1. **Vectorization**: Transform a JSON payload into a 14-dimension float vector using normalization constants from `resources/normalization.json` and MCC risk from `resources/mcc_risk.json`. Spec in `docs/REGRAS_DE_DETECCAO.md`. Key: indices 5 and 6 get sentinel `-1` when `last_transaction` is null.

2. **Vector search**: Find the 5 nearest neighbors in `resources/references.json.gz` (3M labeled vectors, 284 MB uncompressed). Any algorithm allowed (brute-force, ANN, kd-tree, etc.).

3. **Decision**: `fraud_score = fraud_count_in_top5 / 5`. `approved = fraud_score < 0.6`.

### Resource files (at `resources/`)

| File | Size | Usage |
|------|------|-------|
| `normalization.json` | <1 KB | Max values for clamping (amount, installments, km, etc.) |
| `mcc_risk.json` | <1 KB | Risk score by merchant category code (10 entries, default 0.5) |
| `references.json.gz` | ~16 MB gzipped / ~284 MB | 3M reference vectors with `"fraud"`/`"legit"` labels |
| `example-payloads.json` | — | Sample request payloads for testing |
| `example-references.json` | — | Small subset of references for testing |

Pre-process these at build time or startup — they don't change during the test.

## Key constraints from challenge rules

- Load balancer must be dumb round-robin only — no business logic, no payload inspection
- Total resource budget: 1 CPU, 350 MB across all containers
- Must respond on port 9999
- Images must be `linux/amd64` compatible
- Endpoints: `GET /ready` → 2xx, `POST /fraud-score` → `{"approved": bool, "fraud_score": float}`
- Submission goes on `submission` branch with only `docker-compose.yml` + configs (no source)

## Dockerfile notes

- Two-stage: `rust:1.85-alpine` builder → `alpine:3.21` runtime
- Only copies `Cargo.toml` and `src/` to builder — **not** `.cargo/config.toml` or `Cargo.lock`
- Copies `resources/` to runtime image at `/app/resources/`
- Exposes port 9999 but has no default `CMD` — the compose file provides `command`
