# Performance

pgsqlite adds PostgreSQL protocol and translation work on top of SQLite.
Measure the intended workload and configuration rather than relying on historical timings.
SQLite allows one writer at a time; batching writes and using WAL mode may help suitable workloads.
See the [configuration reference](configuration.md) for cache, connection pool, and SQLite options.

## Benchmarks

Run from the repository root:

```bash
./tests/performance/python/run_benchmark.sh
./tests/performance/python/run_driver_comparison.sh
cargo bench -p pgsqlite-tests --bench simple_query_bench
```

The Python benchmarks require Poetry and its configured dependencies.
See the [benchmark guide](../tests/performance/python/README.md) for options and setup.
Some Rust performance tests are ignored by default; run them explicitly with `cargo test -p pgsqlite-tests --test <name> -- --ignored`.
Historical performance reports are retained under `tests/performance/reports/` and do not describe current measurements.