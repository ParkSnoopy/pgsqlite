# Development

pgsqlite implements the PostgreSQL wire protocol over SQLite.
See [architecture](architecture.md), [configuration](configuration.md), and [migrations](migrations.md) for implementation details.
The [backlog](backlog.md) records proposed work.

## Build and verification

Run commands from the repository root:

```bash
cargo check
cargo clippy
cargo build
cargo test
./tests/runner/run_ssl_tests.sh
```

The SQL integration runner requires `psql` and exercises connection modes against a server.
The Python SQLAlchemy suite is `./tests/python/run_sqlalchemy_tests.sh`.
The Rust integration tests and performance tools belong to the `pgsqlite-tests` workspace member under `tests/`.
Use `cargo test -p pgsqlite-tests --test <name>` to run one integration target.
The performance tools are in `tests/performance/`.
Test edge cases and end-to-end behavior, and mark backlog items complete only after implementation and verification.

## Invariants

- Infer types from explicit PostgreSQL declarations, SQLite schema information, or explicit casts; infer from values only as a last resort. Never infer types from column names.
- Store dates as integer days since 1970-01-01. Store times, timestamps, and intervals as integer microseconds from their respective epochs.
- Translate datetime and array expressions in `INSERT ... SELECT`. Decimal aggregate wrapping applies to NUMERIC, not floating-point types.
- Keep session connections isolated. Changes to `__pgsqlite_*` tables require a registered migration with up/down SQL and dependencies; test new and existing databases.
- `array_agg` ordering currently relies on the outer query. Multi-array `unnest` and some catalog paths remain limitations.

Follow existing Rust conventions and keep changes concise.