use rusqlite::{Connection, Result, functions::{Aggregate, FunctionFlags}};
use tracing::debug;

/// Register PostgreSQL catalog-related functions
pub fn register_catalog_functions(conn: &Connection) -> Result<()> {
    debug!("Registering catalog functions");
    
    // pg_table_is_visible(oid) - checks if table is in search path
    // For SQLite, all tables are visible
    conn.create_scalar_function(
        "pg_table_is_visible",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            // Accept either integer or text OID
            // Try to get as i64 first, if that fails try as string and parse
            let _oid = match ctx.get::<i64>(0) {
                Ok(oid) => oid,
                Err(_) => {
                    // Try as string
                    let oid_str: String = ctx.get(0)?;
                    oid_str.parse::<i64>().unwrap_or(0)
                }
            };
            // In SQLite, all tables are visible
            // Return 1 for true (SQLite boolean convention)
            Ok(1i32)
        },
    )?;
    
    // Note: SQLite doesn't support schema-qualified function names,
    // so we handle pg_catalog.pg_table_is_visible through query rewriting
    
    // regclass type cast function
    conn.create_scalar_function(
        "regclass",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let table_name: String = ctx.get(0)?;
            
            // Look up table OID from pg_class view
            // For now, just generate a consistent OID
            let oid = generate_table_oid(&table_name);
            Ok(oid)
        },
    )?;
    
    // __pgsqlite_relnamespace(name) - namespace OID for a relation.
    // Lets catalog views ask "is this relation ours?" by exact name rather than
    // by prefix. Returns NULL for NULL input so a view row degrades rather than
    // failing the query.
    conn.create_scalar_function(
        "__pgsqlite_relnamespace",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let name: Option<String> = ctx.get(0)?;
            Ok(name.map(|n| crate::catalog::internal_relations::relnamespace(&n)))
        },
    )?;

    // Column type metadata for the information_schema.columns view. Four
    // scalar functions rather than one, because a SQLite scalar UDF returns a
    // single value and the view needs four fields from the same input.
    macro_rules! register_column_info_fn {
        ($name:literal, $field:ident) => {
            conn.create_scalar_function(
                $name,
                1,
                FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
                |ctx| {
                    let pg_type: Option<String> = ctx.get(0)?;
                    let value: Option<i32> = pg_type
                        .and_then(|t| crate::catalog::column_type_info::pg_column_info(&t).$field);
                    Ok(value)
                },
            )?;
        };
    }

    conn.create_scalar_function(
        "__pgsqlite_pg_data_type",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pg_type: Option<String> = ctx.get(0)?;
            Ok(pg_type.map(|t| crate::catalog::column_type_info::pg_column_info(&t).data_type))
        },
    )?;

    register_column_info_fn!("__pgsqlite_char_max_length", character_maximum_length);
    register_column_info_fn!("__pgsqlite_numeric_precision", numeric_precision);
    register_column_info_fn!("__pgsqlite_numeric_scale", numeric_scale);

    // Schema type helpers for pg_attribute SQLite view
    conn.create_scalar_function(
        "__pgsqlite_type_oid",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pg_type: Option<String> = ctx.get(0)?;
            Ok(pg_type.map(|t| crate::types::SchemaTypeMapper::pg_type_string_to_oid(&t)))
        },
    )?;

    conn.create_scalar_function(
        "__pgsqlite_type_mod",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pg_type: Option<String> = ctx.get(0)?;
            Ok(pg_type.map(|t| parse_pg_type_mod(&t)).unwrap_or(-1))
        },
    )?;

    conn.create_scalar_function(
        "__pgsqlite_space_array_json",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let val: Option<String> = ctx.get(0)?;
            let Some(s) = val else {
                return Ok("[]".to_string());
            };
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok("[]".to_string());
            }
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                return Ok(trimmed.to_string());
            }
            let elems: Vec<String> = trimmed
                .split(|c: char| c.is_whitespace() || c == ',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| {
                    if part.parse::<i64>().is_ok() {
                        part.to_string()
                    } else {
                        serde_json::to_string(part).unwrap_or_else(|_| format!("\"{part}\""))
                    }
                })
                .collect();
            Ok(format!("[{}]", elems.join(",")))
        },
    )?;

    conn.create_scalar_function(
        "format_type",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let oid = match ctx.get::<i64>(0) {
                Ok(v) => v as i32,
                Err(_) => ctx
                    .get::<Option<String>>(0)?
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(25),
            };
            let typemod = match ctx.get::<Option<i64>>(1) {
                Ok(Some(v)) => v as i32,
                Ok(None) => -1,
                Err(_) => ctx
                    .get::<Option<String>>(1)?
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(-1),
            };
            Ok(format_pg_type(oid, typemod))
        },
    )?;

    conn.create_scalar_function(
        "pg_get_expr",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let expr: Option<String> = ctx.get(0)?;
            Ok(expr)
        },
    )?;

    conn.create_scalar_function(
        "pg_get_serial_sequence",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_ctx| Ok(None::<String>),
    )?;

    conn.create_scalar_function(
        "pg_get_indexdef",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_ctx| Ok(String::new()),
    )?;

    conn.create_scalar_function(
        "pg_get_indexdef",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_ctx| Ok(String::new()),
    )?;

    conn.create_scalar_function(
        "pg_get_constraintdef",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_ctx| Ok(String::new()),
    )?;

    conn.create_scalar_function(
        "pg_get_constraintdef",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_ctx| Ok(String::new()),
    )?;

    conn.create_scalar_function(
        "pg_collation_is_visible",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_ctx| Ok(1i32),
    )?;

    conn.create_scalar_function(
        "pg_type_is_visible",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_ctx| Ok(1i32),
    )?;

    fn lookup_setting(name: &str) -> String {
        match name.to_ascii_lowercase().as_str() {
            "jit" => "off".to_string(),
            "server_version" => "16.0".to_string(),
            "server_version_num" => "160000".to_string(),
            "standard_conforming_strings" => "on".to_string(),
            "client_encoding" | "server_encoding" => "UTF8".to_string(),
            "timezone" => "UTC".to_string(),
            "datestyle" => "ISO, MDY".to_string(),
            "default_transaction_isolation" | "transaction_isolation" => "read committed".to_string(),
            _ => String::new(),
        }
    }

    conn.create_scalar_function(
        "current_setting",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let name: String = ctx.get(0)?;
            Ok(lookup_setting(&name))
        },
    )?;

    conn.create_scalar_function(
        "current_setting",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let name: String = ctx.get(0)?;
            Ok(lookup_setting(&name))
        },
    )?;

    conn.create_scalar_function(
        "set_config",
        3,
        FunctionFlags::SQLITE_UTF8,
        |ctx| {
            let value: String = ctx.get(1)?;
            Ok(value)
        },
    )?;

    #[derive(Default)]
    struct BoolAnd;

    impl Aggregate<Option<bool>, Option<i32>> for BoolAnd {
        fn init(&self, _: &mut rusqlite::functions::Context<'_>) -> Result<Option<bool>> {
            Ok(None)
        }

        fn step(&self, ctx: &mut rusqlite::functions::Context<'_>, agg: &mut Option<bool>) -> Result<()> {
            let b = match ctx.get_raw(0) {
                rusqlite::types::ValueRef::Null => return Ok(()),
                rusqlite::types::ValueRef::Integer(i) => i != 0,
                rusqlite::types::ValueRef::Real(f) => f != 0.0,
                rusqlite::types::ValueRef::Text(bytes) => {
                    let s = std::str::from_utf8(bytes).unwrap_or("").trim();
                    matches!(s, "1" | "t" | "T" | "true" | "TRUE")
                }
                rusqlite::types::ValueRef::Blob(_) => false,
            };
            *agg = Some(agg.unwrap_or(true) && b);
            Ok(())
        }

        fn finalize(&self, _: &mut rusqlite::functions::Context<'_>, agg: Option<Option<bool>>) -> Result<Option<i32>> {
            Ok(agg.flatten().map(|v| if v { 1 } else { 0 }))
        }
    }

    conn.create_aggregate_function("bool_and", 1, FunctionFlags::SQLITE_UTF8, BoolAnd)?;

    debug!("Catalog functions registered successfully");
    Ok(())
}

// Generate a stable OID from table name
fn generate_table_oid(name: &str) -> i32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    ((hasher.finish() & 0x7FFFFFFF) % 1000000 + 16384) as i32
}

fn parse_pg_type_mod(pg_type: &str) -> i32 {
    let trimmed = pg_type.trim();
    let Some(open) = trimmed.find('(') else {
        return -1;
    };
    let Some(close) = trimmed[open + 1..].find(')') else {
        return -1;
    };
    let upper = trimmed[..open].trim().to_ascii_uppercase();
    let args: Vec<&str> = trimmed[open + 1..open + 1 + close]
        .split(',')
        .map(str::trim)
        .collect();
    if upper == "NUMERIC" || upper == "DECIMAL" {
        if let Some(p) = args.first().and_then(|s| s.parse::<i32>().ok()) {
            let s = args.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            return ((p << 16) | (s & 0xFFFF)) + 4;
        }
    } else if matches!(upper.as_str(), "VARCHAR" | "CHARACTER VARYING" | "CHAR" | "CHARACTER" | "BPCHAR")
        && let Some(len) = args.first().and_then(|s| s.parse::<i32>().ok())
    {
        return len + 4;
    }
    -1
}

fn format_pg_type(oid: i32, typemod: i32) -> String {
    match oid {
        16 => "boolean".to_string(),
        17 => "bytea".to_string(),
        18 => "\"char\"".to_string(),
        19 => "name".to_string(),
        20 => "bigint".to_string(),
        21 => "smallint".to_string(),
        23 => "integer".to_string(),
        25 => "text".to_string(),
        26 => "oid".to_string(),
        114 => "json".to_string(),
        700 => "real".to_string(),
        701 => "double precision".to_string(),
        1042 => {
            if typemod > 4 {
                format!("character({})", typemod - 4)
            } else {
                "character".to_string()
            }
        }
        1043 => {
            if typemod > 4 {
                format!("character varying({})", typemod - 4)
            } else {
                "character varying".to_string()
            }
        }
        1082 => "date".to_string(),
        1083 => "time without time zone".to_string(),
        1114 => "timestamp without time zone".to_string(),
        1184 => "timestamp with time zone".to_string(),
        1186 => "interval".to_string(),
        1266 => "time with time zone".to_string(),
        1560 => "bit".to_string(),
        1562 => "bit varying".to_string(),
        1700 => {
            if typemod > 4 {
                let raw = typemod - 4;
                let prec = (raw >> 16) & 0xFFFF;
                let scale = raw & 0xFFFF;
                format!("numeric({prec},{scale})")
            } else {
                "numeric".to_string()
            }
        }
        2950 => "uuid".to_string(),
        3802 => "jsonb".to_string(),
        1000 => "boolean[]".to_string(),
        1001 => "bytea[]".to_string(),
        1005 => "smallint[]".to_string(),
        1007 => "integer[]".to_string(),
        1009 => "text[]".to_string(),
        1014 => "character[]".to_string(),
        1015 => "character varying[]".to_string(),
        1016 => "bigint[]".to_string(),
        1028 => "oid[]".to_string(),
        1021 => "real[]".to_string(),
        1022 => "double precision[]".to_string(),
        1115 => "timestamp without time zone[]".to_string(),
        1182 => "date[]".to_string(),
        1183 => "time without time zone[]".to_string(),
        1185 => "timestamp with time zone[]".to_string(),
        1187 => "interval[]".to_string(),
        1231 => "numeric[]".to_string(),
        1270 => "time with time zone[]".to_string(),
        2951 => "uuid[]".to_string(),
        3807 => "jsonb[]".to_string(),
        199 => "json[]".to_string(),
        _ => "text".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_pg_table_is_visible() {
        let conn = Connection::open_in_memory().unwrap();
        register_catalog_functions(&conn).unwrap();
        
        // Test with any OID - should always return true
        let result: bool = conn
            .query_row("SELECT pg_table_is_visible(12345)", [], |row| row.get(0))
            .unwrap();
        assert!(result);
    }
    
    #[test]
    fn test_regclass_cast() {
        let conn = Connection::open_in_memory().unwrap();
        register_catalog_functions(&conn).unwrap();
        
        // Test regclass cast
        let oid: i32 = conn
            .query_row("SELECT regclass('test_table')", [], |row| row.get(0))
            .unwrap();
        assert!(oid > 0);
        
        // Same table name should produce same OID
        let oid2: i32 = conn
            .query_row("SELECT regclass('test_table')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(oid, oid2);
    }
}