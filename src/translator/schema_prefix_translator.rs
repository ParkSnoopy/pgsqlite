use once_cell::sync::Lazy;
use regex::Regex;
use sqlparser::ast::{Statement, Query, SetExpr, TableFactor, ObjectName, ObjectNamePart};
use tracing::debug;

/// Translator that removes schema prefixes from table names
/// PostgreSQL queries often use schema.table syntax (e.g., pg_catalog.pg_class)
/// but SQLite doesn't support schemas, so we need to strip the prefix
pub struct SchemaPrefixTranslator;

/// Case-insensitively replace every occurrence of `needle` with `replacement`,
/// skipping SQL string literals (`'...'`) and quoted identifiers (`"..."`).
///
/// A blind `str::replace` rewrites matches inside literals, so
/// `SELECT 'information_schema.tables'` used to come back corrupted and a
/// `WHERE msg = 'see pg_catalog.pg_class'` used to stop matching stored rows.
/// Matching is also case-insensitive because the catalog interceptor's gate is,
/// so spellings like `Information_Schema.Tables` reach this translator and must
/// not fall through untranslated to SQLite.
///
/// `needle` must be ASCII. Single left-to-right scan; everything outside a
/// replaced span is preserved byte for byte.
///
/// INVARIANT: callers must strip SQL comments first. This scanner does not
/// recognize `--` or `/* */`, so an apostrophe inside a comment (`-- don't`)
/// would leave it believing the rest of the query is one long string literal
/// and silently skip every real qualifier after it. `strip_sql_comments` runs
/// ahead of this translator on both entry paths (`query::executor` and
/// `query::extended`) and is itself literal-aware, which is what makes that
/// unreachable today.
fn replace_outside_literals(input: &str, needle: &str, replacement: &str) -> String {
    debug_assert!(needle.is_ascii(), "needle must be ASCII for case-insensitive matching");
    if needle.is_empty() || input.len() < needle.len() {
        return input.to_string();
    }

    let bytes = input.as_bytes();
    let needle = needle.as_bytes();
    let mut out = String::with_capacity(input.len());
    // Everything in `input[..copied]` has already been emitted into `out`.
    let mut copied = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let quote = bytes[i];
        if quote == b'\'' || quote == b'"' {
            // Skip the whole quoted span, honoring the SQL doubled-quote escape
            // ('it''s' is one literal). Unterminated spans run to end of input,
            // which leaves them untouched -- the parser rejects them later.
            i += 1;
            while i < bytes.len() {
                if bytes[i] == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if bytes.len() - i >= needle.len() && bytes[i..i + needle.len()].eq_ignore_ascii_case(needle)
        {
            out.push_str(&input[copied..i]);
            out.push_str(replacement);
            i += needle.len();
            copied = i;
            continue;
        }

        i += 1;
    }

    out.push_str(&input[copied..]);
    out
}

impl SchemaPrefixTranslator {
    /// Translate a query string by removing schema prefixes
    pub fn translate_query(query: &str) -> String {
        let mut result = query.to_string();

        // List of known pg_catalog tables that we have views or handlers for
        let catalog_tables = [
            "pg_class", "pg_namespace", "pg_attribute", "pg_type",
            "pg_constraint", "pg_index", "pg_attrdef", "pg_am",
            "pg_enum", "pg_range", "pg_description", "pg_sequence",
            "pg_collation", "pg_opclass", "pg_roles", "pg_user",
            "pg_database", "pg_proc", "pg_depend", "pg_trigger", "pg_settings",
        ];

        for table in &catalog_tables {
            // Replace pg_catalog.table with just table
            result = replace_outside_literals(&result, &format!("pg_catalog.{table}"), table);
        }

        // Also remove schema prefix from functions
        let catalog_functions = [
            "pg_table_is_visible", "pg_get_userbyid", "pg_get_constraintdef",
            "format_type", "pg_get_expr", "pg_get_indexdef", "version",
            "current_database", "current_schema", "current_user", "session_user",
            "pg_backend_pid", "pg_blocking_pids", "pg_is_in_recovery", "current_schemas",
            "pg_get_serial_sequence", "pg_collation_is_visible", "pg_type_is_visible",
        ];

        for func in &catalog_functions {
            result = replace_outside_literals(&result, &format!("pg_catalog.{func}"), func);
        }

        // information_schema relations exist as SQLite views with underscores.
        // Rewriting here routes them to the views through the interceptor's
        // fall-through path, the same way pg_catalog.* reaches pg_class.
        // Only the two relations served by views; the rest still have handlers.
        result = replace_outside_literals(&result, "information_schema.tables", "information_schema_tables");
        result = replace_outside_literals(&result, "information_schema.columns", "information_schema_columns");

        debug!("Schema prefix translation: {} -> {}", query, result);
        result
    }

    /// Rewrite `pg_table_is_visible(<pg_class_or_alias>.oid)` into `<alias>.relnamespace != 13000`
    /// so `information_schema` relations (`relnamespace = 13000`) stay hidden in default visibility checks.
    pub fn translate_pg_table_is_visible_for_pg_class(query: &str) -> String {
        if !query.contains("pg_class") || !query.contains("pg_table_is_visible") {
            return query.to_string();
        }
        static PG_CLASS_VIS_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"(?i)(?:pg_catalog\.)?pg_table_is_visible\s*\(\s*(?:(?:pg_catalog\.)?([a-zA-Z_][a-zA-Z0-9_]*))\.oid\s*\)").unwrap()
        });
        PG_CLASS_VIS_RE
            .replace_all(query, "($1.relnamespace != 13000)")
            .to_string()
    }

    /// Rewrite PostgreSQL reflection constructs that have no direct SQLite equivalent
    /// (bare `'t'`/`'f'` truthiness in `pg_attribute`, `pg_get_constraintdef`,
    /// ENUM `format_type`, SRF `unnest`/`generate_subscripts` over `pg_index`, and TEXT/INT OID joins).
    pub fn translate_reflection_sql(query: &str) -> String {
        if !query.contains("pg_") {
            return query.to_string();
        }
        let mut result = query.to_string();

        if result.contains("atthasdef") {
            static WHEN_ATTHASDEF_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"(?i)\bWHEN\s+((?:[a-zA-Z_]\w*\.)?atthasdef)\s+THEN\b").unwrap()
            });
            static WHERE_ATTHASDEF_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"(?i)\b(WHERE|AND)\s+((?:[a-zA-Z_]\w*\.)?atthasdef)\b(?:\s*(?:!=|<>|=|IS\b))?").unwrap()
            });
            result = WHEN_ATTHASDEF_RE
                .replace_all(&result, "WHEN ($1 IN ('t', 1, 'true')) THEN")
                .to_string();
            result = WHERE_ATTHASDEF_RE
                .replace_all(&result, |caps: &regex::Captures| {
                    let full = caps.get(0).unwrap().as_str();
                    let trimmed_end = full.trim_end();
                    if trimmed_end.ends_with('=')
                        || trimmed_end.ends_with("<>")
                        || trimmed_end.to_ascii_uppercase().ends_with("IS")
                    {
                        full.to_string()
                    } else {
                        format!("{} ({} IN ('t', 1, 'true'))", &caps[1], &caps[2])
                    }
                })
                .to_string();
        }

        if result.contains("attisdropped") {
            static NOT_ATTISDROPPED_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"(?i)\bNOT\s+((?:[a-zA-Z_]\w*\.)?attisdropped)\b").unwrap()
            });
            result = NOT_ATTISDROPPED_RE
                .replace_all(&result, "($1 IN ('f', 0, 'false'))")
                .to_string();
        }

        if result.contains("format_type(")
            && result.contains("pg_attribute")
            && !result.contains("typtype = 'e' LIMIT 1), format_type(")
        {
            static FORMAT_TYPE_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"(?i)\bformat_type\s*\(\s*([^,]+),\s*([^)]+)\)").unwrap()
            });
            result = FORMAT_TYPE_RE
                .replace_all(
                    &result,
                    "COALESCE((SELECT typname FROM pg_type WHERE CAST(oid AS INTEGER) = CAST($1 AS INTEGER) AND typtype = 'e' LIMIT 1), format_type($1, $2))",
                )
                .to_string();
        }

        if result.contains("pg_get_constraintdef(") {
            static GET_CONSTRAINTDEF_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"(?i)\bpg_get_constraintdef\s*\(\s*([a-zA-Z_]\w*)\.oid\s*(?:,[^)]+)?\)").unwrap()
            });
            result = GET_CONSTRAINTDEF_RE
                .replace_all(&result, "COALESCE($1.consrc, '')")
                .to_string();
        }

        if result.contains("generate_subscripts") && result.contains("pg_index") {
            static CONSTRAINT_SRF_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(
                    r"(?is)SELECT\s+pg_constraint\.conrelid\s+AS\s+conrelid,\s*pg_constraint\.conname\s+AS\s+conname,\s*(?:(?:pg_catalog\.)?unnest\s*\(\s*pg_index\.indkey\s*\)|\(\s*SELECT\s+value\s+FROM\s+json_each\s*\(\s*pg_index\.indkey\s*\)\s*\))\s+AS\s+attnum,\s*(?:pg_catalog\.)?generate_subscripts\s*\(\s*pg_index\.indkey\s*,\s*(?:[^()]+|\([^()]*\))+\)\s+AS\s+ord,\s*pg_index\.indnkeyatts\s+AS\s+indnkeyatts,\s*pg_index\.indnullsnotdistinct\s+AS\s+indnullsnotdistinct,\s*pg_description\.description\s+AS\s+description\s+FROM\s+pg_constraint\s+JOIN\s+pg_index\s+ON\s+pg_constraint\.conindid\s*=\s*pg_index\.indexrelid"
                ).unwrap()
            });
            result = CONSTRAINT_SRF_RE
                .replace_all(
                    &result,
                    "SELECT pg_constraint.conrelid AS conrelid, pg_constraint.conname AS conname, CAST(_pgi_je.value AS INTEGER) AS attnum, CAST(_pgi_je.key AS INTEGER) AS ord, pg_index.indnkeyatts AS indnkeyatts, pg_index.indnullsnotdistinct AS indnullsnotdistinct, pg_description.description AS description FROM pg_constraint JOIN pg_index ON CAST(pg_constraint.conindid AS INTEGER) = CAST(pg_index.indexrelid AS INTEGER) JOIN json_each(__pgsqlite_space_array_json(pg_index.indkey)) AS _pgi_je ON 1=1",
                )
                .to_string();

            static INDEX_SRF_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(
                    r"(?is)SELECT\s+pg_index\.indexrelid\s+AS\s+indexrelid,\s*pg_index\.indrelid\s+AS\s+indrelid,\s*(?:(?:pg_catalog\.)?unnest\s*\(\s*pg_index\.indkey\s*\)|\(\s*SELECT\s+value\s+FROM\s+json_each\s*\(\s*pg_index\.indkey\s*\)\s*\))\s+AS\s+attnum,\s*(?:(?:pg_catalog\.)?unnest\s*\(\s*pg_index\.indclass\s*\)|\(\s*SELECT\s+value\s+FROM\s+json_each\s*\(\s*pg_index\.indclass\s*\)\s*\))\s+AS\s+att_opclass,\s*(?:pg_catalog\.)?generate_subscripts\s*\(\s*pg_index\.indkey\s*,\s*(?:[^()]+|\([^()]*\))+\)\s+AS\s+ord\s+FROM\s+pg_index\b"
                ).unwrap()
            });
            result = INDEX_SRF_RE
                .replace_all(
                    &result,
                    "SELECT pg_index.indexrelid AS indexrelid, pg_index.indrelid AS indrelid, CAST(_pgi_je.value AS INTEGER) AS attnum, COALESCE(CAST(json_extract(__pgsqlite_space_array_json(pg_index.indclass), '$[' || _pgi_je.key || ']') AS INTEGER), 0) AS att_opclass, CAST(_pgi_je.key AS INTEGER) AS ord FROM pg_index JOIN json_each(__pgsqlite_space_array_json(pg_index.indkey)) AS _pgi_je ON 1=1",
                )
                .to_string();

            static IN_OID_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"(?i)\b((?:pg_constraint|pg_index|con|idx|idx_attr)\.(?:conrelid|indrelid))\s+IN\s*\(([^)]+)\)").unwrap()
            });
            result = IN_OID_RE
                .replace_all(&result, "(CAST($1 AS INTEGER) IN ($2) OR CAST($1 AS TEXT) IN ($2))")
                .to_string();
        }

        result
    }

    /// Translate an AST by removing schema prefixes
    pub fn translate_statement(stmt: &mut Statement) -> Result<(), sqlparser::parser::ParserError> {
        match stmt {
            Statement::Query(query) => Self::translate_query_ast(query),
            _ => Ok(()),
        }
    }
    
    fn translate_query_ast(query: &mut Query) -> Result<(), sqlparser::parser::ParserError> {
        if let SetExpr::Select(select) = &mut *query.body {
            // Translate table names in FROM clause
            for table_ref in &mut select.from {
                Self::translate_table_factor(&mut table_ref.relation)?;
                
                // Also handle JOINs
                for join in &mut table_ref.joins {
                    Self::translate_table_factor(&mut join.relation)?;
                }
            }
        }
        Ok(())
    }
    
    fn translate_table_factor(factor: &mut TableFactor) -> Result<(), sqlparser::parser::ParserError> {
        if let TableFactor::Table { name, .. } = factor {
            Self::translate_object_name(name);
        }
        Ok(())
    }
    
    fn translate_object_name(name: &mut ObjectName) {
        // If the name has 2 parts (schema.table), remove the schema part
        if name.0.len() == 2 {
            let schema = &name.0[0];
            let table = &name.0[1];
            
            // Check if it's a pg_catalog schema
            let ObjectNamePart::Identifier(schema) = schema else {
                return;
            };
            let schema_name = schema.value.to_lowercase();
            
            if schema_name == "pg_catalog" {
                // Replace with just the table name
                name.0 = vec![table.clone()];
            } else if schema_name == "information_schema" {
                // The two relations served by SQLite views are rewritten to
                // their underscore names; the rest keep their Rust handlers.
                let ObjectNamePart::Identifier(table) = table else {
                    return;
                };
                let table_name = table.value.to_lowercase();
                if table_name == "tables" || table_name == "columns" {
                    let mut ident = table.clone();
                    ident.value = format!("information_schema_{table_name}");
                    name.0 = vec![ObjectNamePart::Identifier(ident)];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_schema_prefix_removal() {
        let query = "SELECT * FROM pg_catalog.pg_class WHERE relname = 'test'";
        let translated = SchemaPrefixTranslator::translate_query(query);
        assert_eq!(translated, "SELECT * FROM pg_class WHERE relname = 'test'");
    }
    
    #[test]
    fn test_function_prefix_removal() {
        let query = "SELECT pg_catalog.pg_table_is_visible(oid) FROM pg_catalog.pg_class";
        let translated = SchemaPrefixTranslator::translate_query(query);
        assert_eq!(translated, "SELECT pg_table_is_visible(oid) FROM pg_class");
    }
    
    #[test]
    fn test_join_prefix_removal() {
        let query = "SELECT * FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid";
        let translated = SchemaPrefixTranslator::translate_query(query);
        assert_eq!(translated, "SELECT * FROM pg_class c JOIN pg_namespace n ON c.relnamespace = n.oid");
    }

    #[test]
    fn test_information_schema_tables_rewrite() {
        let query = "SELECT table_name FROM information_schema.tables ORDER BY 1";
        let translated = SchemaPrefixTranslator::translate_query(query);
        assert_eq!(translated, "SELECT table_name FROM information_schema_tables ORDER BY 1");
    }

    #[test]
    fn test_information_schema_columns_rewrite() {
        let query = "SELECT * FROM information_schema.columns";
        let translated = SchemaPrefixTranslator::translate_query(query);
        assert_eq!(translated, "SELECT * FROM information_schema_columns");
    }

    /// Relations that still have Rust handlers must not be rewritten -- there is
    /// no `information_schema_routines` view to fall through to.
    #[test]
    fn test_other_information_schema_relations_are_untouched() {
        for relation in ["routines", "views", "triggers", "check_constraints"] {
            let query = format!("SELECT * FROM information_schema.{relation}");
            assert_eq!(SchemaPrefixTranslator::translate_query(&query), query);
        }
    }

    /// `table_constraints` shares a prefix with `tables` -- verify no collision.
    #[test]
    fn test_table_constraints_is_not_caught_by_the_tables_rewrite() {
        let query = "SELECT * FROM information_schema.table_constraints";
        assert_eq!(SchemaPrefixTranslator::translate_query(query), query);
    }

    /// The catalog interceptor gates on a lowercased query, so mixed-case
    /// spellings reach the translator and must not fall through to SQLite.
    #[test]
    fn test_mixed_case_schema_qualifiers_are_rewritten() {
        for query in [
            "SELECT * FROM Information_Schema.Tables",
            "SELECT * FROM information_schema.TABLES",
            "SELECT * FROM INFORMATION_SCHEMA.tables",
            "SELECT * FROM INFORMATION_SCHEMA.TABLES",
        ] {
            assert_eq!(
                SchemaPrefixTranslator::translate_query(query),
                "SELECT * FROM information_schema_tables",
                "failed for {query}"
            );
        }

        assert_eq!(
            SchemaPrefixTranslator::translate_query("SELECT * FROM Information_Schema.Columns"),
            "SELECT * FROM information_schema_columns"
        );
        assert_eq!(
            SchemaPrefixTranslator::translate_query("SELECT * FROM Pg_Catalog.Pg_Class"),
            "SELECT * FROM pg_class"
        );
    }

    /// A blind `str::replace` rewrote matches inside string literals, silently
    /// corrupting stored SQL text and breaking equality comparisons.
    #[test]
    fn test_string_literals_are_not_rewritten() {
        for query in [
            "SELECT 'information_schema.tables' AS s",
            "SELECT 'information_schema.columns' AS s",
            "SELECT 'pg_catalog.pg_class' AS s",
            "INSERT INTO notes (msg) VALUES ('see information_schema.tables for details')",
            "SELECT * FROM notes WHERE msg = 'see pg_catalog.pg_class for details'",
        ] {
            assert_eq!(SchemaPrefixTranslator::translate_query(query), query);
        }
    }

    /// `'it''s'` is one literal, not two -- a naive scan would treat the text
    /// after the doubled quote as unquoted and rewrite it.
    #[test]
    fn test_doubled_quote_escape_keeps_the_literal_intact() {
        let query = "SELECT 'it''s information_schema.tables' AS s";
        assert_eq!(SchemaPrefixTranslator::translate_query(query), query);
    }

    /// Double quotes are identifier quoting in PostgreSQL, so a quoted
    /// identifier is one literal name and never a schema qualifier.
    #[test]
    fn test_quoted_identifiers_are_not_rewritten() {
        let query = r#"SELECT * FROM "information_schema.tables""#;
        assert_eq!(SchemaPrefixTranslator::translate_query(query), query);

        let query = r#"SELECT * FROM "pg_catalog.pg_class""#;
        assert_eq!(SchemaPrefixTranslator::translate_query(query), query);
    }

    /// Text outside the replaced spans, including non-ASCII, is preserved.
    #[test]
    fn test_rewrites_outside_literals_still_happen_alongside_literals() {
        let query = "SELECT 'information_schema.tables' AS lbl FROM information_schema.tables WHERE table_name = 'naïve.pg_catalog.pg_class'";
        assert_eq!(
            SchemaPrefixTranslator::translate_query(query),
            "SELECT 'information_schema.tables' AS lbl FROM information_schema_tables WHERE table_name = 'naïve.pg_catalog.pg_class'"
        );
    }

    #[test]
    fn test_unterminated_literal_is_left_alone() {
        let query = "SELECT 'information_schema.tables";
        assert_eq!(SchemaPrefixTranslator::translate_query(query), query);
    }
}