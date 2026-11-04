# Lead Radar backend

Rust modular monolith: one crate, four binaries.

| Binary | Purpose |
|---|---|
| `api` | Axum HTTP API under `/api/v1` (+ `/health`, `/ready`, `/api/v1/openapi.json`) |
| `worker` | durable SQLite-backed job loop with bounded concurrency |
| `db` | `migrate`, `seed`, `purge [days]`, `reset --confirm`, `token <plaintext>`, `settings` |
| `healthcheck` | container probe for `/health` |

## Quick start

```bash
cp ../.env.example ../.env     # once; fill only the credentials you have
cargo run --bin db migrate
cargo run --bin db seed
cargo run --bin api            # :8080
cargo run --bin worker         # second terminal
```

Offline mode: `DECISION_PROVIDER=mock GENERATIVE_PROVIDER=mock SOURCE_PROVIDER=mock`.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## Layout

`src/api` (HTTP), `src/modules` (domain modules with SQL), `src/domain` (pure logic),
`src/providers` (external adapters behind traits), `src/worker` (job handlers), `migrations/`
(versioned SQL). Rules and conventions: `AGENTS.md`. Architecture: `../docs/architecture.md`.

## License

PolyForm Noncommercial License 1.0.0 (see `LICENSE`). Noncommercial use is free; commercial use needs a
paid license, described in the root repository's `COMMERCIAL.md`.
