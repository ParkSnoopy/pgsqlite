use rusqlite::Connection;
use anyhow::Result;
use tracing::{debug, info};
use once_cell::sync::Lazy;
use regex::Regex;

// Pre-compiled regex patterns for constraint parsing
static PK_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:[(,]\s*)([a-zA-Z_]\w*)\s+(?:[^,\(\)]+|\([^()]*\))*\bPRIMARY\s+KEY\b").unwrap()
});

static TABLE_PK_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)PRIMARY\s+KEY\s*\(\s*([^)]+)\s*\)").unwrap()
});

static UNIQUE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(\w+)\s+[^,\)]*\bUNIQUE\b").unwrap()
});

static TABLE_UNIQUE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:CONSTRAINT\s+(\w+)\s+)?UNIQUE\s*\(\s*([^)]+)\s*\)").unwrap()
});

static CHECK_START_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:CONSTRAINT\s+(\w+)\s+)?CHECK\s*\(").unwrap()
});

static DEFAULT_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(\w+)\s+[^,\)]*\bDEFAULT\s+([^,\)]+)").unwrap()
});

static FOREIGN_KEY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:CONSTRAINT\s+(\w+)\s+)?FOREIGN\s+KEY\s*\(\s*([^)]+)\s*\)\s+REFERENCES\s+(\w+)\s*\(\s*([^)]+)\s*\)").unwrap()
});

static INLINE_FOREIGN_KEY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(\w+)\s+[^,\)]*\bREFERENCES\s+(\w+)\s*\(\s*([^)]+)\s*\)").unwrap()
});

static TABLE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)CREATE\s+TABLE\s+[^(]+\(\s*(.+)\s*\)").unwrap()
});

/// Populate PostgreSQL catalog tables with constraint information for a newly created table
pub fn populate_constraints_for_table(conn: &Connection, table_name: &str) -> Result<()> {
    eprintln!("🎯 populate_constraints_for_table called for: {}", table_name);
    info!("Populating constraints for table: {}", table_name);

    // Get the CREATE TABLE statement from SQLite
    let create_sql = get_create_table_sql(conn, table_name)?;
    debug!("CREATE TABLE SQL: {}", create_sql);
    
    // Generate table OID (consistent with pg_class view)
    let table_oid = generate_table_oid(table_name);

    // Parse and populate constraints
    populate_table_constraints(conn, table_name, &create_sql, &table_oid)?;

    // Parse and populate column defaults
    populate_column_defaults(conn, table_name, &create_sql, &table_oid)?;

    // Populate indexes (including those created by UNIQUE constraints)
    populate_table_indexes(conn, table_name, &table_oid)?;

    // Populate dependencies (for Rails sequence ownership detection)
    populate_table_dependencies(conn, table_name, &table_oid)?;

    info!("Successfully populated constraints for table: {}", table_name);
    Ok(())
}

/// Refresh `pg_index` and constraint index links for a table (e.g. after `CREATE INDEX`).
pub fn populate_index_for_table(conn: &Connection, table_name: &str) -> Result<()> {
    let table_oid = generate_table_oid(table_name);
    populate_table_indexes(conn, table_name, &table_oid)
}

/// Extract target table name from `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON table_name (...)`.
pub fn extract_table_name_from_create_index(query: &str) -> Option<String> {
    static CREATE_INDEX_TABLE_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?i)CREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:IF\s+NOT\s+EXISTS\s+)?(?:"[^"]+"|\w+)\s+ON\s+(?:"([^"]+)"|([a-zA-Z_][a-zA-Z0-9_]*))"#).unwrap()
    });
    let caps = CREATE_INDEX_TABLE_RE.captures(query)?;
    caps.get(1)
        .or_else(|| caps.get(2))
        .map(|m| m.as_str().to_string())
}

/// Get the CREATE TABLE statement for a table from sqlite_master
fn get_create_table_sql(conn: &Connection, table_name: &str) -> Result<String> {
    let mut stmt = conn.prepare("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    let sql: String = stmt.query_row([table_name], |row| row.get(0))?;
    Ok(sql)
}

/// Generate table OID using the same algorithm as the pg_class view
fn generate_table_oid(name: &str) -> String {
    // Must match the formula in pg_class view for JOIN compatibility. Delegates to the
    // shared canonical implementation in crate::utils so every OID producer in the tree
    // (views, catalog handlers, and this populator) agrees.
    crate::utils::generate_table_oid(name).to_string()
}

/// Generate constraint OID with better collision avoidance
fn generate_constraint_oid(name: &str, contype: &str) -> String {
    use crate::utils::generate_oid;
    // Add the constraint type to the name to avoid collisions between different constraint types
    let unique_name = format!("{}_{}", name, contype);
    // Use a different offset range for constraints to avoid collision with tables
    let base_oid = generate_oid(&unique_name);
    // Offset by 500000 to put constraints in a different range
    let final_oid = base_oid + 500000;
    eprintln!("  🔑 OID generation: {} + {} -> base:{} final:{}", name, contype, base_oid, final_oid);
    final_oid.to_string()
}

/// Extract referenced table name from foreign key definition and return its OID
fn get_referenced_table_oid(_conn: &Connection, definition: &str) -> Result<String> {
    // Extract table name from "FOREIGN KEY REFERENCES table_name(column)"
    if let Some(cap) = Regex::new(r"(?i)REFERENCES\s+(\w+)").unwrap().captures(definition)
        && let Some(table_name) = cap.get(1) {
            // Use the same formula as pg_class view for consistency
            return Ok(generate_table_oid(table_name.as_str()));
        }

    // Fallback to a default OID if parsing fails
    Ok("0".to_string())
}

/// Populate pg_constraint table with constraint information
fn populate_table_constraints(conn: &Connection, table_name: &str, create_sql: &str, table_oid: &str) -> Result<()> {
    let constraints = parse_table_constraints(table_name, create_sql);
    eprintln!("🔎 Found {} constraints for table {}", constraints.len(), table_name);
    for c in &constraints {
        eprintln!("  - {} (type: {})", c.name, c.contype);
    }

    for constraint in constraints {
        if constraint.contype == "f" {
            // Foreign key constraint - needs additional fields
            info!("Found foreign key constraint: {} for column: {:?}", constraint.name, constraint.columns);
            let ref_table_oid = get_referenced_table_oid(conn, &constraint.definition)?;
            info!("Referenced table OID: {}", ref_table_oid);

            // Convert column names to column numbers for conkey
            let col_nums: Vec<String> = constraint.columns
                .iter()
                .filter_map(|col_name| get_column_number(create_sql, col_name))
                .map(|n| n.to_string())
                .collect();

            // Check if this OID already exists
            let existing: Result<String, _> = conn.query_row(
                "SELECT conname FROM pg_constraint WHERE oid = ?1",
                [&constraint.oid],
                |row| row.get(0)
            );
            if let Ok(existing_name) = existing {
                eprintln!("  ⚠️ OID {} already exists for constraint: {}", constraint.oid, existing_name);
            }

            eprintln!("💾 Inserting foreign key: oid={}, name={}, conrelid={}, confrelid={}, conkey={}, confkey={}",
                     constraint.oid, constraint.name, table_oid, ref_table_oid, col_nums.join(","), "1");
            let result = conn.execute(
                "INSERT OR IGNORE INTO pg_constraint (
                    oid, conname, contype, conrelid, confrelid, conkey, confkey, consrc,
                    confupdtype, confdeltype, confmatchtype, conislocal, convalidated
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    constraint.oid,                           // oid as TEXT
                    constraint.name,
                    constraint.contype,
                    table_oid,                               // conrelid as TEXT
                    ref_table_oid,                           // confrelid as TEXT (to match pg_class.oid)
                    col_nums.join(","),    // Column numbers as comma-separated list (no braces for LIKE pattern)
                    "1".to_string(),       // Default to column 1 of referenced table
                    constraint.definition,
                    "a".to_string(),   // NO ACTION (default)
                    "a".to_string(),   // NO ACTION (default)
                    "s".to_string(),   // SIMPLE (default)
                    true,              // conislocal as boolean
                    true,              // convalidated as boolean
                ]
            );
            match result {
                Ok(n) => eprintln!("  ✅ Inserted {} row(s)", n),
                Err(e) => eprintln!("  ❌ Failed to insert: {}", e),
            }
        } else {
            // Other constraint types - also convert to column numbers
            let col_nums: Vec<String> = constraint.columns
                .iter()
                .filter_map(|col_name| get_column_number(create_sql, col_name))
                .map(|n| n.to_string())
                .collect();

            conn.execute(
                "INSERT OR IGNORE INTO pg_constraint (
                    oid, conname, contype, conrelid, conkey, consrc, conislocal, convalidated
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    constraint.oid,     // oid as TEXT
                    constraint.name,
                    constraint.contype,
                    table_oid,         // conrelid as TEXT
                    col_nums.join(","),    // Column numbers as comma-separated list (no braces for LIKE pattern)
                    constraint.definition,
                    true,              // conislocal as boolean
                    true,              // convalidated as boolean
                ]
            )?;
        }
        
        debug!("Inserted constraint: {} (type: {}) for table: {}", 
               constraint.name, constraint.contype, table_name);
    }
    
    Ok(())
}

/// Populate pg_attrdef table with column default information
fn populate_column_defaults(conn: &Connection, table_name: &str, create_sql: &str, table_oid: &str) -> Result<()> {
    let defaults = parse_column_defaults(table_name, create_sql);
    
    for default in defaults {
        conn.execute(
            "INSERT OR IGNORE INTO pg_attrdef (
                oid, adrelid, adnum, adbin, adsrc
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
            [
                &default.oid,
                table_oid,
                &default.column_num.to_string(),
                &default.default_expr,
                &default.default_expr,
            ]
        )?;
        
        debug!("Inserted default: column {} = '{}' for table: {}", 
               default.column_num, default.default_expr, table_name);
    }
    
    Ok(())
}

/// Populate pg_index table with index information
fn populate_table_indexes(conn: &Connection, table_name: &str, table_oid: &str) -> Result<()> {
    // First, get table columns to map column names to numbers (1-based like PostgreSQL)
    let mut column_map = std::collections::HashMap::new();
    let query = format!("PRAGMA table_info({})", table_name);
    let mut column_stmt = conn.prepare(&query)?;
    let column_rows = column_stmt.query_map([], |row| {
        let cid: i32 = row.get(0)?;
        let name: String = row.get(1)?;
        Ok((name, cid + 1)) // Convert to 1-based like PostgreSQL attnum
    })?;

    for column_result in column_rows {
        let (name, attnum) = column_result?;
        column_map.insert(name, attnum);
    }

    // Get indexes using PRAGMA index_list
    let query = format!("PRAGMA index_list({})", table_name);
    let mut index_stmt = conn.prepare(&query)?;
    let index_rows = index_stmt.query_map([], |row| {
        let seq: i32 = row.get(0)?;
        let name: String = row.get(1)?;
        let is_unique: bool = row.get(2)?;
        let origin: String = row.get(3)?;
        let partial: bool = row.get(4)?;
        Ok((seq, name, is_unique, origin, partial))
    })?;

    let mut constraint_indexes: Vec<(String, String, bool, bool)> = Vec::new();

    for index_result in index_rows {
        let (_seq, index_name, is_unique, origin, _partial) = index_result?;
        let index_oid = crate::utils::generate_table_oid(&index_name).to_string();

        // Get column information for this index using PRAGMA index_info
        let query = format!("PRAGMA index_info({})", index_name);
        let mut info_stmt = conn.prepare(&query)?;
        let info_rows = info_stmt.query_map([], |row| {
            let seqno: i32 = row.get(0)?;
            let cid: i32 = row.get(1)?;
            let name: Option<String> = row.get(2)?;
            Ok((seqno, cid, name))
        })?;

        let mut column_numbers = Vec::new();
        let mut column_count = 0;

        for info_result in info_rows {
            let (_seqno, _cid, col_name_opt) = info_result?;
            if let Some(col_name) = col_name_opt
                && let Some(&attnum) = column_map.get(&col_name) {
                column_numbers.push(attnum.to_string());
                column_count += 1;
            }
        }

        let indkey_csv = column_numbers.join(",");
        let indkey = column_numbers.join(" ");
        let indoption = vec!["0"; column_count.max(1)].join(" ");

        // Determine if this is a primary key index
        let is_primary = origin == "pk" || index_name.contains("primary") || index_name.contains("pkey");
        if is_primary || origin == "u" {
            constraint_indexes.push((index_oid.clone(), indkey_csv.clone(), is_primary, is_unique));
        }

        // Skip sqlite_autoindex rows from user-index queries ONLY if not backing a constraint?
        // Actually SQLAlchemy keeps constraint-backed indexes linked via conindid (`has_constraint`),
        // except sqlite_autoindex should only be recorded when backing PK/UNIQUE constraints so
        // get_indexes() returns explicit indexes while get_unique_constraints()/get_pk_constraint() work.
        if index_name.starts_with("sqlite_") && !is_primary && origin != "u" {
            continue;
        }

        conn.execute(
            "INSERT OR REPLACE INTO pg_index (
                indexrelid, indrelid, indnatts, indnkeyatts,
                indisunique, indisprimary, indkey,
                indisexclusion, indimmediate, indisclustered,
                indisvalid, indcheckxmin, indisready, indislive,
                indisreplident, indcollation, indclass, indoption,
                indexprs, indpred
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 1, 0, 1, 0, 1, 1, 0, '', '', ?8, '', '')",
            rusqlite::params![
                &index_oid,
                table_oid,
                column_count as i32,
                column_count as i32, // For regular indexes, indnkeyatts = indnatts
                is_unique as i32,
                is_primary as i32,
                &indkey,
                &indoption,
            ]
        )?;

        debug!("Inserted index: {} (unique: {}, primary: {}, columns: {}) for table: {}",
               index_name, is_unique, is_primary, indkey, table_name);
    }

    // Link pg_constraint PK/UNIQUE rows to matching pg_index entries, creating synthetic pg_index
    // rows for INTEGER PRIMARY KEY (which SQLite backs via rowid rather than sqlite_autoindex).
    let mut con_stmt = conn.prepare(
        "SELECT oid, conname, contype, conkey FROM pg_constraint WHERE conrelid = ?1 AND contype IN ('p', 'u')"
    )?;
    let con_rows: Vec<(String, String, String, String)> = con_stmt
        .query_map([table_oid], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3).unwrap_or_default(),
            ))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(con_stmt);

    for (con_oid, con_name, contype, conkey_csv) in con_rows {
        let is_pk = contype == "p";
        let matched_idx_oid = constraint_indexes
            .iter()
            .find(|(_, idx_csv, pk, _)| *pk == is_pk && *idx_csv == conkey_csv)
            .map(|(oid, _, _, _)| oid.clone());

        let idx_oid = if let Some(oid) = matched_idx_oid {
            oid
        } else {
            let synthetic_oid = crate::utils::generate_table_oid(&format!("__idx_{table_name}_{con_name}")).to_string();
            let col_nums: Vec<&str> = conkey_csv
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let n_cols = col_nums.len().max(1) as i32;
            let indkey = col_nums.join(" ");
            let indoption = vec!["0"; n_cols.max(1) as usize].join(" ");
            conn.execute(
                "INSERT OR IGNORE INTO pg_index (
                    indexrelid, indrelid, indnatts, indnkeyatts,
                    indisunique, indisprimary, indkey,
                    indisexclusion, indimmediate, indisclustered,
                    indisvalid, indcheckxmin, indisready, indislive,
                    indisreplident, indcollation, indclass, indoption,
                    indexprs, indpred
                ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, 0, 1, 0, 1, 0, 1, 1, 0, '', '', ?7, '', '')",
                rusqlite::params![
                    &synthetic_oid,
                    table_oid,
                    n_cols,
                    n_cols,
                    is_pk as i32,
                    &indkey,
                    &indoption,
                ]
            )?;
            synthetic_oid
        };

        let _ = conn.execute(
            "UPDATE pg_constraint SET conindid = CAST(?1 AS INTEGER) WHERE oid = ?2",
            rusqlite::params![&idx_oid, &con_oid],
        );
    }

    Ok(())
}

/// Information about a constraint
#[derive(Debug)]
struct ConstraintInfo {
    oid: String,
    name: String,
    contype: String,
    columns: Vec<String>,
    definition: String,
}

/// Information about a column default
#[derive(Debug)]
struct DefaultInfo {
    oid: String,
    column_num: i16,
    default_expr: String,
}

/// Parse table constraints from CREATE TABLE statement
fn parse_table_constraints(table_name: &str, create_sql: &str) -> Vec<ConstraintInfo> {
    let mut constraints = Vec::new();
    info!("Parsing constraints for table: {} from SQL: {}", table_name, create_sql);

    // Parse PRIMARY KEY constraints
    // Look for both inline PRIMARY KEY and table-level PRIMARY KEY
    for cap in PK_REGEX.captures_iter(create_sql) {
        if let Some(column_name) = cap.get(1) {
            let col = column_name.as_str();
            if matches!(
                col.to_ascii_uppercase().as_str(),
                "CREATE" | "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
            ) {
                continue;
            }
            constraints.push(ConstraintInfo {
                oid: generate_constraint_oid(&format!("{table_name}_pkey"), "p"),
                name: format!("{table_name}_pkey"),
                contype: "p".to_string(),
                columns: vec![col.to_string()],
                definition: "PRIMARY KEY".to_string(),
            });
        }
    }
    
    // Parse table-level PRIMARY KEY constraints (avoid duplicating inline PK if already captured)
    for cap in TABLE_PK_REGEX.captures_iter(create_sql) {
        if let Some(columns_str) = cap.get(1) {
            let columns: Vec<String> = columns_str.as_str()
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !columns.is_empty() && !constraints.iter().any(|c| c.contype == "p") {
                constraints.push(ConstraintInfo {
                    oid: generate_constraint_oid(&format!("{table_name}_pkey"), "p"),
                    name: format!("{table_name}_pkey"),
                    contype: "p".to_string(),
                    columns,
                    definition: "PRIMARY KEY".to_string(),
                });
            }
        }
    }
    
    // Parse UNIQUE constraints
    for cap in UNIQUE_REGEX.captures_iter(create_sql) {
        if let Some(column_name) = cap.get(1) {
            constraints.push(ConstraintInfo {
                oid: generate_constraint_oid(&format!("{}_{}_key", table_name, column_name.as_str()), "u"),
                name: format!("{}_{}_key", table_name, column_name.as_str()),
                contype: "u".to_string(),
                columns: vec![column_name.as_str().to_string()],
                definition: "UNIQUE".to_string(),
            });
        }
    }
    
    // Parse table-level UNIQUE constraints
    for cap in TABLE_UNIQUE_REGEX.captures_iter(create_sql) {
        if let Some(columns_str) = cap.get(2) {
            let columns: Vec<String> = columns_str.as_str()
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .collect();
            let constraint_name = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| format!("{}_{}_key", table_name, columns.join("_")));
            constraints.push(ConstraintInfo {
                oid: generate_constraint_oid(&constraint_name, "u"),
                name: constraint_name,
                contype: "u".to_string(),
                columns,
                definition: "UNIQUE".to_string(),
            });
        }
    }
    
    // Parse CHECK constraints using balanced parenthesis matching
    for (i, cap) in CHECK_START_REGEX.captures_iter(create_sql).enumerate() {
        let m = cap.get(0).unwrap();
        let open_paren_pos = m.end() - 1;
        let rest = &create_sql[open_paren_pos..];
        let mut depth = 0usize;
        let mut in_sq = false;
        let mut end_idx = None;
        for (idx, ch) in rest.char_indices() {
            match ch {
                '\'' => in_sq = !in_sq,
                '(' if !in_sq => depth += 1,
                ')' if !in_sq => {
                    depth -= 1;
                    if depth == 0 {
                        end_idx = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(end) = end_idx {
            let expr = rest[1..end].trim();
            let constraint_name = cap
                .get(1)
                .map(|g| g.as_str().to_string())
                .unwrap_or_else(|| format!("{}_check{}", table_name, i + 1));
            constraints.push(ConstraintInfo {
                oid: generate_constraint_oid(&constraint_name, "c"),
                name: constraint_name,
                contype: "c".to_string(),
                columns: vec![],
                definition: format!("CHECK ({expr})"),
            });
        }
    }

    // Parse table-level FOREIGN KEY constraints
    for cap in FOREIGN_KEY_REGEX.captures_iter(create_sql) {
        if let (Some(local_columns), Some(ref_table), Some(ref_columns)) =
            (cap.get(2), cap.get(3), cap.get(4)) {
            let local_cols: Vec<String> = local_columns.as_str()
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .collect();
            let ref_cols: Vec<String> = ref_columns.as_str()
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .collect();

            let constraint_name = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| format!("{}_{}_fkey", table_name, local_cols.join("_")));
            constraints.push(ConstraintInfo {
                oid: generate_constraint_oid(&constraint_name, "f"),
                name: constraint_name,
                contype: "f".to_string(),
                columns: local_cols.clone(),
                definition: format!(
                    "FOREIGN KEY ({}) REFERENCES {}({})",
                    local_cols.join(", "),
                    ref_table.as_str(),
                    ref_cols.join(", ")
                ),
            });
        }
    }

    // Parse inline FOREIGN KEY constraints (column REFERENCES table(column))
    info!("Checking for inline foreign keys with regex");
    for cap in INLINE_FOREIGN_KEY_REGEX.captures_iter(create_sql) {
        if let (Some(column_name), Some(ref_table), Some(ref_column)) =
            (cap.get(1), cap.get(2), cap.get(3)) {
            info!("Found inline foreign key: {} REFERENCES {}({})", column_name.as_str(), ref_table.as_str(), ref_column.as_str());
            let constraint_name = format!("{}_{}_fkey", table_name, column_name.as_str());
            constraints.push(ConstraintInfo {
                oid: generate_constraint_oid(&constraint_name, "f"),
                name: constraint_name,
                contype: "f".to_string(),
                columns: vec![column_name.as_str().to_string()],
                definition: format!(
                    "FOREIGN KEY ({}) REFERENCES {}({})",
                    column_name.as_str(),
                    ref_table.as_str(),
                    ref_column.as_str()
                ),
            });
        }
    }

    constraints
}

/// Parse column defaults from CREATE TABLE statement
fn parse_column_defaults(table_name: &str, create_sql: &str) -> Vec<DefaultInfo> {
    let mut defaults = Vec::new();
    
    // Parse DEFAULT clauses - look for column definitions with DEFAULT
    for cap in DEFAULT_REGEX.captures_iter(create_sql) {
        if let (Some(column_name), Some(default_value)) = (cap.get(1), cap.get(2)) {
            // Get column number by counting columns before this one
            let column_num = get_column_number(create_sql, column_name.as_str()).unwrap_or(1);
            
            defaults.push(DefaultInfo {
                oid: generate_constraint_oid(&format!("{}_{}_default", table_name, column_name.as_str()), "d"),
                column_num,
                default_expr: default_value.as_str().trim().to_string(),
            });
        }
    }
    
    defaults
}

fn first_col_ident(col_def: &str) -> &str {
    col_def
        .trim_start()
        .trim_start_matches('"')
        .split(|c: char| c.is_whitespace() || c == '"')
        .next()
        .unwrap_or("")
}

/// Get the column number (1-based) for a given column name in a CREATE TABLE statement
fn get_column_number(create_sql: &str, target_column: &str) -> Option<i16> {
    let target_clean = target_column.trim().trim_matches('"');
    // Extract the column definitions from CREATE TABLE
    if let Some(cap) = TABLE_REGEX.captures(create_sql)
        && let Some(columns_part) = cap.get(1) {
            // Split by comma and look for our target column
            let columns_str = columns_part.as_str();
            let mut column_count = 0i16;
            
            // Simple column parsing - split by commas but be careful of nested parentheses
            let mut paren_depth = 0;
            let mut current_column = String::new();
            
            for ch in columns_str.chars() {
                match ch {
                    '(' => {
                        paren_depth += 1;
                        current_column.push(ch);
                    }
                    ')' => {
                        paren_depth -= 1;
                        current_column.push(ch);
                    }
                    ',' if paren_depth == 0 => {
                        // End of column definition
                        column_count += 1;
                        if first_col_ident(&current_column).eq_ignore_ascii_case(target_clean) {
                            return Some(column_count);
                        }
                        current_column.clear();
                    }
                    _ => {
                        current_column.push(ch);
                    }
                }
            }
            
            // Check the last column
            if !current_column.trim().is_empty() {
                column_count += 1;
                if first_col_ident(&current_column).eq_ignore_ascii_case(target_clean) {
                    return Some(column_count);
                }
            }
        }
    
    None
}

/// Populate pg_depend table with dependency information for sequence ownership detection
fn populate_table_dependencies(conn: &Connection, table_name: &str, table_oid: &str) -> Result<()> {
    debug!("Populating dependencies for table: {}", table_name);

    // Get table columns to find INTEGER PRIMARY KEY (acts like SERIAL in SQLite)
    let query = format!("PRAGMA table_info({})", table_name);
    let mut column_stmt = conn.prepare(&query)?;
    let column_rows = column_stmt.query_map([], |row| {
        let cid: i32 = row.get(0)?;
        let name: String = row.get(1)?;
        let column_type: String = row.get(2)?;
        let pk: i32 = row.get(5)?;
        Ok((cid, name, column_type, pk))
    })?;

    let mut pk_columns = Vec::new();
    for (cid, column_name, column_type, pk) in column_rows.flatten() {
        debug!("Column: {} (cid={}, type={}, pk={}) in table {}", column_name, cid, column_type, pk, table_name);
        if pk > 0 {
            pk_columns.push((cid, column_name.clone(), column_type.clone(), pk));
        }
    }

    // Only create dependencies for single-column INTEGER PRIMARY KEY
    if pk_columns.len() == 1 {
        let (cid, column_name, column_type, _pk) = &pk_columns[0];
        if column_type.to_uppercase().contains("INTEGER") {
            debug!("Found single INTEGER PRIMARY KEY column: {} in table {} at position {}", column_name, table_name, cid + 1);

            // Generate deterministic OIDs
            let sequence_oid = generate_sequence_oid(table_name, column_name);
            let table_oid_str = table_oid; // table_oid is already a string

            // Insert dependency record into pg_depend table
            // This represents: sequence depends on table column (automatic dependency)
            let result = conn.execute(
                "INSERT OR REPLACE INTO pg_depend (classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "1259",        // classid: pg_class OID (for sequences)
                    sequence_oid.to_string(),  // objid: sequence OID
                    "0",           // objsubid: 0 for sequences
                    "1259",        // refclassid: pg_class OID (for tables)
                    table_oid_str,             // refobjid: table OID
                    cid + 1,       // refobjsubid: column number (1-based like PostgreSQL)
                    "a"            // deptype: automatic dependency
                ],
            )?;

            debug!("Inserted pg_depend record: sequence {} depends on column {} of table {} (result: {})",
                   sequence_oid, column_name, table_name, result);
        }
    } else {
        debug!("Table {} has {} PK columns, skipping dependency creation", table_name, pk_columns.len());
    }

    Ok(())
}

/// Generate a deterministic OID for a sequence based on table and column name
fn generate_sequence_oid(table_name: &str, column_name: &str) -> u32 {
    use crate::utils::generate_oid;
    let sequence_name = format!("{}_{}_seq", table_name, column_name);
    // Use the standard OID generator but offset for sequences to avoid conflicts
    generate_oid(&sequence_name) + 50000
}