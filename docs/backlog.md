# Backlog

These items are retained from the historical backlog and require reassessment against the current implementation before work begins.

## Protocol and types

- [ ] Complete prepared-statement lifecycle, named statements, and `DEALLOCATE`.
- [ ] Add `COPY TO` and `COPY FROM`, including text, binary, and CSV formats.
- [ ] Review cursor `FETCH` and Execute row limits against existing portal support.
- [ ] Complete binary encoders for ranges, network, bit, and full-text search types.
- [ ] Support datetime infinities, month/year intervals, IANA timezones, timezone caching, and a datetime migration guide.
- [ ] Support aggregate-level `array_agg` ordering, multi-array `unnest`, array assignment and comparisons, and advanced array functions (`generate_subscripts`, `array_dims`, `array_fill`, `cardinality`, and `width_bucket`).
- [ ] Improve array indexing, content search, and large-array performance; expand set-returning functions.
- [ ] Add JSONPath and geometric types, operators, and functions.

## SQL and catalogs

- [ ] Review support for materialized CTEs, lateral joins, window frames and exclusions, grouping sets, values tables, and MERGE.
- [ ] Improve catalog joins, namespace visibility, index and constraint metadata, type conversions in complex joins, and catalog query performance.
- [ ] Review `regclass`, object and column comments, additional catalogs and information schema views, `psql` discovery commands, and PostgreSQL system functions.
- [ ] Support `CREATE EXTENSION` and improve PostgreSQL error codes, positions, hints, details, and constraint errors.
- [ ] Review exact catalog namespacing, `pg_attribute` type OIDs, view and ENUM columns, and remaining information-schema handlers ([issue #102](https://github.com/erans/pgsqlite/issues/102)).
- [ ] Review stable shared OIDs and unknown predicates under `NOT` in remaining catalog handlers.
- [ ] Fix typed `CREATE TABLE ... AS SELECT` handling and assess internal-table filtering for that form.
- [ ] Review catalog aggregate handling and remaining catalog paths that use a shared mutable connection.
- [ ] Review transaction isolation and `set_config` behavior, including transaction-local settings and quoted values.

## Performance and storage

- [ ] Investigate cached SELECT regressions, INSERT overhead, and binary-protocol performance for complex types.
- [ ] Review expression and partial indexes, multicolumn statistics, index-only scans, cost-based planning, join ordering, subquery unnesting, and common-subexpression elimination.
- [ ] Explore compression, sparse columns, partitioning, and vacuum/analyze equivalents.

## Security and operations

- [ ] Review row-level and column-level permissions, authentication methods, TLS modes, client certificates, and certificate rotation.
- [ ] Improve query and pool statistics, metrics export, and slow-query logging.
- [ ] Review runtime parameters, configuration files, per-database settings, connection limits, and timeouts.

## Verification and documentation

- [ ] Expand type-conversion and protocol-compliance coverage, performance benchmarks, and concurrent-connection stress tests.
- [ ] Review API, PostgreSQL migration, performance tuning, and troubleshooting documentation.