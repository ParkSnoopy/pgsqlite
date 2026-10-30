# PostgreSQL Compatibility TODO

Track remaining PostgreSQL wire protocol, type serialization, and query compatibility improvements.

---

## 1. Boolean Text Serialization (`\x00` -> `'t'` / `'f'`)
- **Status**: Open
- **Description**:
  When PostgreSQL clients request results in text format (format code 0), boolean values must be serialized as ASCII `'t'` or `'f'`. SQLite native booleans return `0` / `1` or byte `\x00`, which causes client drivers (such as Go `pgx` and `lib/pq`) to report:
  - `Scan error on column index N: couldn't convert "\x00" into type bool`
  - `unknown boolean string representation "\x00"`
- **Affected Area**:
  - `src/query/extended.rs` (`ExtendedQueryHandler::encode_row`)
  - `src/session/db_handler.rs`
- **Action**:
  Encode boolean column values (type OID 16 / Bool) as `'t'` or `'f'` in text format mode.

---

## 2. Timestamp Driver Value Scanning & OID Typing
- **Status**: Open
- **Description**:
  Timestamp expressions and columns scanned into native driver time structures report:
  - `Scan error: unsupported Scan, storing driver.Value type string into type *time.Time`
- **Affected Area**:
  - `src/types/schema_type_mapper.rs`
  - `src/query/extended.rs`
- **Action**:
  Ensure timestamp expressions and columns resolve to appropriate PostgreSQL type OIDs (1114 for `timestamp`, 1184 for `timestamptz`), and format text values according to standard ISO-8601 / RFC 3339 layouts.

---

## 3. PostgreSQL SQL Built-in Functions & Expressions
- **Status**: Open
- **Sub-tasks**:
  1. **`TO_CHAR` Function Support**:
     - Implement `to_char(timestamp, format)` UDF in `src/functions/datetime_functions.rs` to support standard date-time formatting expressions.
  2. **Multi-argument `CAST` Support**:
     - Handle dialect-specific cast variations like `CAST(val, default AS type)` in `src/query/executor.rs` or `src/translator/cast_translator.rs`.
  3. **Table Alias Expansion in JSON Functions (`to_jsonb(alias)`)**:
     - When queries call `to_jsonb(table_alias)` in joins, SQLite raises `no such column: <alias>`. Expand whole-table references into JSON object constructors in `src/translator/json_translator.rs`.
  4. **`array_contains` Expression Translation**:
     - Translate PostgreSQL `array_contains(...)` function calls into SQLite-compatible JSON array expressions in `src/translator/array_translator.rs`.

---

## 4. Complex Subquery and Result Set Handling
- **Status**: Open
- **Description**:
  Ensure queries with nested JSON sub-selects and conditional aggregations return non-nil result rows with complete column definitions across extended query execution cycles.
- **Affected Area**:
  - `src/query/extended.rs`
  - `src/query/executor.rs`

---

## 5. Array Membership Parameter Evaluation
- **Status**: Open
- **Description**:
  Queries using parameter placeholders with array membership checks (e.g., `SELECT id, $2 = ANY(array_col) WHERE ...`) must preserve parameter bindings and accurately evaluate scalar-in-array conditions.
- **Affected Area**:
  - `src/translator/array_translator.rs`
  - `src/session/db_handler.rs`
