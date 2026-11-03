# AGENTS.md — lead-radar-backend

Rust modular monolith: one crate, two binaries (`api`, `worker`), shared library.
Pin the toolchain in `rust-toolchain.toml`. Never upgrade a dependency inside a
feature commit.

## Boundaries

- `src/api/**` — HTTP only: routing, extractors, DTOs, validation, error mapping.
  No business rules, no SQL.
- `src/modules/**` — one directory per domain module (tenancy, sources,
  ingestion, decision, generative, qualification, comments, engagement,
  operations). Each owns its models, repository and domain rules.
- `src/domain/**` — pure logic. No I/O, no async, no clock reads unless injected.
- `src/providers/**` — external provider adapters behind traits. Provider-specific
  payloads never leak outside the adapter.
- `src/worker/**` — job loop and handlers. Handlers call module services; they do
  not talk to `sqlx` directly.

## Mandatory rules

1. **Tenant scoping.** Every query touching tenant-owned data carries
   `tenant_id` from the authenticated `TenantContext`. A `tenant_id` coming from
   a request body, query string or path is a bug. Cross-tenant isolation has a
   dedicated test.
2. **No transaction across I/O.** Never hold a SQLite transaction while calling
   Apify, Jev, Laya or Qwen. Persist intent first, then act, then record outcome.
3. **Single writer.** Worker writes go through the bounded writer pool; SQLite
   allows one writer. Separating ingestion concurrency from AI concurrency is
   required.
4. **Idempotency.** Ingestion and lead creation are idempotent by stored keys
   (`external_id`, `content_hash`, `idempotency_key`). Unique constraints enforce
   it in the database, not only in code.
5. **Money.** Cost estimates and actual billed usage are stored in separate
   columns and never conflated. Unknown cost is `NULL`, never `0`.
6. **Provider neutrality.** `DecisionProvider` and `SourceProvider` are the only
   seams. A new provider must not require changes in domain modules.
7. **Error handling.** `thiserror` for domain errors, `anyhow` only in binaries
   and tests. `unwrap`/`expect` are forbidden in request paths; use `expect` only
   for provable invariants with a message.
8. **Secrets.** Read from config, never logged. Log request metadata, not
   headers or bodies containing credentials.

## Testing

- `cargo test` must pass offline. No paid API calls: `wiremock` for HTTP,
  in-memory temp SQLite per test, deterministic fixtures.
- Domain rules (scoring, thresholds, dedup, job state machine, probability
  handling) need unit tests with boundaries and failure cases.
- Integration tests cover migrations, seeds idempotency, tenant isolation,
  ingestion, run state transitions, cost guards, retries and recovery.
- Coverage target: 85% of domain modules (`cargo llvm-cov --summary-only`).

## Style

- `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`.
- Public items documented where the contract is not obvious.
- No `unsafe`.

## Commits

Local only. Conventional Commits. State `Risk: none` or `Risk: item N (reason)`
in the body; never push.
