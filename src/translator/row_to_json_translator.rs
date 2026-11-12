use crate::types::PgType;
use crate::translator::metadata::{TranslationMetadata, ColumnTypeHint};

/// Translator for PostgreSQL row_to_json() function calls
pub struct RowToJsonTranslator;

impl RowToJsonTranslator {
    /// Translate row_to_json(t) calls to JSON object construction
    pub fn translate_row_to_json(query: &str) -> (String, TranslationMetadata) {
        Self::translate_row_to_json_with_conn(query, None)
    }

    /// Translate row_to_json(t) / to_json(t) / to_jsonb(t) calls with optional DB connection for table alias expansion
    pub fn translate_row_to_json_with_conn(
        query: &str,
        conn: Option<&rusqlite::Connection>,
    ) -> (String, TranslationMetadata) {
        let mut metadata = TranslationMetadata::new();

        // Look for row_to_json / to_json / to_jsonb patterns in the query
        let mut translated_query = if Self::contains_row_to_json_call(query) {
            Self::translate_query_with_conn(query, conn, &mut metadata)
        } else {
            query.to_string()
        };

        if let Some(conn) = conn {
            if Self::contains_json_populate_record_call(&translated_query) {
                translated_query = Self::translate_json_populate_record_with_conn(&translated_query, conn);
            }
            if Self::contains_update_table_alias(&translated_query) {
                translated_query = Self::translate_update_table_alias(&translated_query);
            }
        }

        if translated_query.contains("make_interval") || translated_query.contains("MAKE_INTERVAL") {
            translated_query = crate::translator::DateTimeTranslator::translate_make_interval(&translated_query);
        }

        (translated_query, metadata)
    }

    /// Check if query contains row_to_json, to_json, to_jsonb, json_populate_record, or UPDATE table alias
    pub fn contains_row_to_json_call(query: &str) -> bool {
        let query_lower = query.to_lowercase();
        query_lower.contains("row_to_json(")
            || query_lower.contains("to_jsonb(")
            || query_lower.contains("to_json(")
            || query_lower.contains("json_populate_record(")
            || query_lower.contains("json_populate_recordset(")
            || (query_lower.contains("update ") && query_lower.contains(" set"))
            || query_lower.contains("make_interval(")
    }

    /// Translate the query by converting row_to_json calls
    fn translate_query_with_conn(
        query: &str,
        conn: Option<&rusqlite::Connection>,
        metadata: &mut TranslationMetadata,
    ) -> String {
        // Look for subquery pattern first: SELECT row_to_json(t) FROM (SELECT ...) t
        if let Some(translated) = Self::translate_subquery_pattern(query, metadata) {
            return translated;
        }

        // If we have a connection, expand table aliases in row_to_json(alias) / to_jsonb(alias) / to_json(alias)
        if let Some(conn) = conn
            && let Some(translated) = Self::translate_table_alias_pattern(query, conn, metadata)
        {
            return translated;
        }

        query.to_string()
    }

    /// Expand row_to_json(alias), to_jsonb(alias), or to_json(alias) where alias refers to a table in FROM/JOIN
    fn translate_table_alias_pattern(
        query: &str,
        conn: &rusqlite::Connection,
        metadata: &mut TranslationMetadata,
    ) -> Option<String> {
        use regex::Regex;

        let call_re = Regex::new(
            r"(?i)(?:CAST\s*\(\s*(row_to_json|to_jsonb|to_json)\s*\(\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*\)\s+AS\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\)|(row_to_json|to_jsonb|to_json)\s*\(\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*\)(?:::([a-zA-Z_][a-zA-Z0-9_]*))?)"
        ).ok()?;
        if !call_re.is_match(query) {
            return None;
        }

        // Map alias -> table_name from FROM / JOIN clauses
        let table_re = Regex::new(
            r"(?i)\b(?:FROM|JOIN)\s+([a-zA-Z_][a-zA-Z0-9_]*)(?:\s+(?:AS\s+)?([a-zA-Z_][a-zA-Z0-9_]*))?\b",
        )
        .ok()?;
        let reserved = [
            "ON", "WHERE", "LEFT", "RIGHT", "INNER", "OUTER", "CROSS", "JOIN", "ORDER", "GROUP",
            "LIMIT", "OFFSET", "HAVING", "UNION", "SET",
        ];
        let mut alias_map: std::collections::HashMap<
            String,
            (String, Option<Vec<(String, Option<String>)>>),
        > = std::collections::HashMap::new();
        for caps in table_re.captures_iter(query) {
            let table_name = caps.get(1)?.as_str();
            alias_map.insert(table_name.to_lowercase(), (table_name.to_string(), None));
            if let Some(alias_match) = caps.get(2) {
                let alias_str = alias_match.as_str();
                if !reserved.contains(&alias_str.to_uppercase().as_str()) {
                    alias_map.insert(alias_str.to_lowercase(), (table_name.to_string(), None));
                }
            }
        }

        // Also map alias -> table_name for subqueries in FROM/JOIN clauses
        for (alias, table_name, specific_cols) in Self::extract_from_subqueries(query) {
            if !reserved.contains(&alias.to_uppercase().as_str()) {
                alias_map.insert(alias.to_lowercase(), (table_name, specific_cols));
            }
        }

        let mut result = query.to_string();
        let mut changed = false;

        for caps in call_re.captures_iter(query) {
            let full_match = caps.get(0)?.as_str();
            let (fn_name, alias, cast_type) = if let Some(f) = caps.get(1) {
                (
                    f.as_str().to_lowercase(),
                    caps.get(2)?.as_str(),
                    caps.get(3).map(|m| m.as_str().to_uppercase()),
                )
            } else {
                (
                    caps.get(4)?.as_str().to_lowercase(),
                    caps.get(5)?.as_str(),
                    caps.get(6).map(|m| m.as_str().to_uppercase()),
                )
            };
            let Some((table_name, specific_cols)) = alias_map.get(&alias.to_lowercase()) else {
                continue;
            };

            // Query columns from PRAGMA table_info if table_name is not empty
            let all_cols: Vec<(String, String)> = if !table_name.is_empty() {
                let pragma_sql = format!("PRAGMA table_info(\"{table_name}\")");
                if let Ok(mut stmt) = conn.prepare(&pragma_sql) {
                    stmt.query_map([], |row| {
                        Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                    })
                    .map(|mapped| mapped.filter_map(Result::ok).collect())
                    .unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            let cols: Vec<(String, String)> = if let Some(wanted) = specific_cols {
                wanted
                    .iter()
                    .map(|(w, inferred)| {
                        let t = if let Some(inf) = inferred {
                            inf.clone()
                        } else {
                            all_cols
                                .iter()
                                .find(|(name, _)| name.eq_ignore_ascii_case(w))
                                .map(|(_, t)| t.clone())
                                .unwrap_or_default()
                        };
                        (w.clone(), t)
                    })
                    .collect()
            } else {
                all_cols
            };

            if cols.is_empty() {
                continue;
            }

            // Also fetch pg_type from __pgsqlite_schema if available
            let mut pg_types = std::collections::HashMap::new();
            if !table_name.is_empty()
                && let Ok(mut schema_stmt) = conn.prepare(
                    "SELECT column_name, pg_type FROM __pgsqlite_schema WHERE LOWER(table_name) = LOWER(?1)",
                )
                && let Ok(rows) = schema_stmt.query_map([table_name], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
            {
                for (c, t) in rows.flatten() {
                    pg_types.insert(c.to_lowercase(), t.to_uppercase());
                }
            }

            let mut json_args = Vec::with_capacity(cols.len() * 2);
            for (col_name, sqlite_type) in &cols {
                let eff_type = pg_types
                    .get(&col_name.to_lowercase())
                    .cloned()
                    .unwrap_or_else(|| sqlite_type.to_uppercase());
                let val_expr = if eff_type.ends_with("[]") {
                    format!("CASE WHEN {alias}.{col_name} IS NULL THEN NULL ELSE json(array_to_json({alias}.{col_name})) END")
                } else if eff_type.contains("JSON") {
                    format!("CASE WHEN {alias}.{col_name} IS NULL THEN NULL ELSE json({alias}.{col_name}) END")
                } else if eff_type == "BOOL" || eff_type == "BOOLEAN" {
                    format!(
                        "CASE WHEN {alias}.{col_name} IS NULL THEN NULL WHEN {alias}.{col_name} IN (1, '1', 't', 'true') THEN json('true') ELSE json('false') END"
                    )
                } else if eff_type.contains("TIMESTAMP")
                    || eff_type.contains("DATE")
                    || col_name.ends_with("_at")
                    || col_name.ends_with("_used")
                    || col_name.ends_with("_seen")
                    || col_name.ends_with("_time")
                    || col_name == "ts"
                    || col_name == "created"
                    || col_name == "updated"
                {
                    format!("pgsqlite_to_rfc3339({alias}.{col_name})")
                } else {
                    format!("{alias}.{col_name}")
                };
                json_args.push(format!("'{col_name}', {val_expr}"));
            }

            let has_id = cols.iter().any(|(name, _)| name == "id");
            let replacement = if has_id {
                format!(
                    "CASE WHEN {alias}.id IS NULL THEN NULL ELSE json_object({}) END",
                    json_args.join(", ")
                )
            } else {
                format!("json_object({})", json_args.join(", "))
            };
            result = result.replace(full_match, &replacement);
            let suggested = if let Some(ref ct) = cast_type {
                match ct.as_str() {
                    "TEXT" | "VARCHAR" => PgType::Text,
                    "JSON" => PgType::Json,
                    "JSONB" => PgType::Jsonb,
                    _ => {
                        if fn_name == "to_jsonb" {
                            PgType::Jsonb
                        } else {
                            PgType::Json
                        }
                    }
                }
            } else if fn_name == "to_jsonb" {
                PgType::Jsonb
            } else {
                PgType::Json
            };
            metadata.add_hint(
                alias.to_string(),
                ColumnTypeHint {
                    source_column: Some(alias.to_string()),
                    suggested_type: Some(suggested),
                    datetime_subtype: None,
                    is_expression: true,
                    expression_type: None,
                },
            );
            metadata.add_hint(
                "row_to_json".to_string(),
                ColumnTypeHint {
                    source_column: None,
                    suggested_type: Some(suggested),
                    datetime_subtype: None,
                    is_expression: true,
                    expression_type: None,
                },
            );
            changed = true;
        }

        if changed {
            Some(result)
        } else {
            None
        }
    }
    
    /// Translate subquery patterns like SELECT row_to_json(t) FROM (SELECT ...) t
    fn translate_subquery_pattern(query: &str, metadata: &mut TranslationMetadata) -> Option<String> {
        use regex::Regex;
        
        // Pattern to match: row_to_json(alias) FROM (...) alias
        // We'll use a two-step approach since backreferences may not work as expected
        let pattern = r"(?i)row_to_json\s*\(\s*(\w+)\s*\)\s+FROM\s+\(\s*(.+?)\s*\)\s+(\w+)";
        
        if let Ok(re) = Regex::new(pattern)
            && let Some(captures) = re.captures(query) {
                let alias1 = captures.get(1).map(|m| m.as_str()).unwrap_or("t");
                let subquery = captures.get(2).map(|m| m.as_str()).unwrap_or("");
                let alias2 = captures.get(3).map(|m| m.as_str()).unwrap_or("t");
                
                // Check if the aliases match (this is our backreference check)
                if alias1 != alias2 {
                    return None;
                }
                
                let alias = alias1;
                
                // Extract column names from the subquery
                if let Some(columns) = Self::extract_columns_from_select(subquery) {
                    if columns.iter().any(|c| c.contains('*')) {
                        return None;
                    }
                    // Build JSON object construction
                    let json_fields: Vec<String> = columns.iter()
                        .map(|col| format!("'{col}', {col}"))
                        .collect();
                    
                    let json_construction = format!("json_object({})", json_fields.join(", "));
                    
                    // Replace the row_to_json call with json_object
                    let result = query.replace(
                        &format!("row_to_json({alias})"),
                        &json_construction
                    );
                    
                    // Add metadata hint for the result type
                    let hint = ColumnTypeHint {
                        source_column: Some(alias.to_string()),
                        suggested_type: Some(PgType::Json),
                        datetime_subtype: None,
                        is_expression: true,
                        expression_type: None,
                    };
                    metadata.add_hint(alias.to_string(), hint);
                    
                    return Some(result);
                }
            }
        
        None
    }
    
    /// Extract column names from a SELECT clause
    fn extract_columns_from_select(select_clause: &str) -> Option<Vec<String>> {
        // Simple extraction of column names from SELECT clause
        // This is a basic implementation - a full parser would be more robust
        
        if let Some(select_start) = select_clause.to_lowercase().find("select") {
            let after_select = &select_clause[select_start + 6..].trim();
            
            // Find the FROM clause to get the column part
            let columns_part = if let Some(from_pos) = after_select.to_lowercase().find(" from ") {
                &after_select[..from_pos]
            } else {
                after_select
            };
            
            // Split by comma and extract column names
            let columns: Vec<String> = columns_part
                .split(',')
                .map(|col| {
                    let col = col.trim();
                    // Handle aliases (col AS alias or col alias)
                    if let Some(as_pos) = col.to_lowercase().find(" as ") {
                        col[as_pos + 4..].trim().to_string()
                    } else {
                        // Simple column name (might have spaces for implicit alias)
                        let parts: Vec<&str> = col.split_whitespace().collect();
                        if parts.len() >= 2 {
                            parts[parts.len() - 1].to_string()
                        } else {
                            col.to_string()
                        }
                    }
                })
                .collect();
            
            return Some(columns);
        }
        
        None
    }

    /// Extract subquery aliases from FROM/JOIN (SELECT ... FROM <table> ...) [AS] <alias>
    fn extract_from_subqueries(
        query: &str,
    ) -> Vec<(String, String, Option<Vec<(String, Option<String>)>>)> {
        let mut results = Vec::new();
        let query_bytes = query.as_bytes();
        let len = query.len();
        let mut pos = 0;

        while pos < len {
            let remaining = &query[pos..];
            let from_or_join = if let Some(idx) = remaining.to_lowercase().find("from") {
                Some((pos + idx, 4))
            } else if let Some(idx) = remaining.to_lowercase().find("join") {
                Some((pos + idx, 4))
            } else {
                None
            };

            let Some((keyword_pos, keyword_len)) = from_or_join else {
                break;
            };

            // Check word boundary before keyword
            if keyword_pos > 0 && (query_bytes[keyword_pos - 1].is_ascii_alphanumeric() || query_bytes[keyword_pos - 1] == b'_') {
                pos = keyword_pos + keyword_len;
                continue;
            }

            // Check word boundary after keyword
            let after_kw = keyword_pos + keyword_len;
            if after_kw < len && (query_bytes[after_kw].is_ascii_alphanumeric() || query_bytes[after_kw] == b'_') {
                pos = after_kw;
                continue;
            }

            // Skip whitespace after keyword
            let mut i = after_kw;
            while i < len && query_bytes[i].is_ascii_whitespace() {
                i += 1;
            }

            if i < len && query_bytes[i] == b'(' {
                let open_paren = i;
                let mut depth = 1;
                i += 1;
                while i < len && depth > 0 {
                    if query_bytes[i] == b'(' {
                        depth += 1;
                    } else if query_bytes[i] == b')' {
                        depth -= 1;
                    }
                    i += 1;
                }

                if depth == 0 {
                    let close_paren = i - 1;
                    let subquery = query[open_paren + 1..close_paren].trim();

                    // Parse alias after close paren
                    while i < len && query_bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    // Optional AS
                    if i + 2 <= len && query[i..].to_lowercase().starts_with("as") {
                        let after_as = i + 2;
                        if after_as == len || query_bytes[after_as].is_ascii_whitespace() {
                            i = after_as;
                            while i < len && query_bytes[i].is_ascii_whitespace() {
                                i += 1;
                            }
                        }
                    }
                    // Read alias identifier
                    let alias_start = i;
                    while i < len && (query_bytes[i].is_ascii_alphanumeric() || query_bytes[i] == b'_') {
                        i += 1;
                    }
                    if i > alias_start {
                        let alias = &query[alias_start..i];

                        // Parse subquery: SELECT <cols> FROM <table>
                        if let Some(select_idx) = Self::find_top_level_keyword(subquery, "select") {
                            let after_select = subquery[select_idx + 6..].trim();
                            if let Some(from_idx) = Self::find_top_level_keyword(after_select, "from") {
                                let select_cols_str = after_select[..from_idx].trim();
                                let after_from = after_select[from_idx + 4..].trim();

                                // Read table name (handling optional quotes)
                                let (table_name, _) = if after_from.starts_with('"') {
                                    if let Some(close_q) = after_from[1..].find('"') {
                                        (&after_from[1..1 + close_q], &after_from[1 + close_q + 1..])
                                    } else {
                                        ("", "")
                                    }
                                } else {
                                    let mut t_end = 0;
                                    let after_from_bytes = after_from.as_bytes();
                                    while t_end < after_from.len() && (after_from_bytes[t_end].is_ascii_alphanumeric() || after_from_bytes[t_end] == b'_') {
                                        t_end += 1;
                                    }
                                    (&after_from[..t_end], &after_from[t_end..])
                                };

                                let items = Self::split_top_level_comma(select_cols_str);
                                let has_wildcard = items.iter().any(|item| {
                                    let item = item.trim();
                                    item == "*" || item.ends_with(".*")
                                });

                                let specific_cols = if has_wildcard {
                                    None
                                } else {
                                    let mut cols = Vec::new();
                                    for item in items {
                                        let item = item.trim();
                                        if item.is_empty() {
                                            continue;
                                        }
                                        let item_upper = item.to_uppercase();
                                        let inferred_type = if item_upper.contains("ARRAY_AGG")
                                            || item_upper.contains("ARRAY[")
                                            || item_upper.contains("JSON_AGG")
                                            || item_upper.contains("JSONB_AGG")
                                            || item_upper.contains("[]")
                                            || item_upper.contains("::JSON")
                                        {
                                            Some("TEXT[]".to_string())
                                        } else if item_upper.contains("COUNT(")
                                            || item_upper.contains("SUM(")
                                            || item_upper.contains("ROW_NUMBER(")
                                        {
                                            Some("INTEGER".to_string())
                                        } else if item_upper.contains("NOW()")
                                            || item_upper.contains("CURRENT_TIMESTAMP")
                                            || item_upper.contains("(TS)")
                                            || item_upper.contains(".TS")
                                            || item_upper.contains("_AT")
                                            || item_upper.contains("MAX(TS)")
                                            || item_upper.contains("MIN(TS)")
                                        {
                                            Some("TIMESTAMP".to_string())
                                        } else {
                                            None
                                        };

                                        // Look for AS alias at top level
                                        if let Some(as_pos) = Self::find_top_level_keyword(item, "as") {
                                            let col_alias = item[as_pos + 2..].trim().trim_matches(|c| c == '"' || c == '`');
                                            if !col_alias.is_empty() {
                                                cols.push((col_alias.to_string(), inferred_type));
                                                continue;
                                            }
                                        }
                                        // Or space before last identifier
                                        let parts: Vec<&str> = item.split_whitespace().collect();
                                        if parts.len() >= 2 && !parts.last().unwrap().contains(')') {
                                            let col_alias = parts.last().unwrap().trim_matches(|c| c == '"' || c == '`');
                                            cols.push((col_alias.to_string(), inferred_type));
                                        } else {
                                            // Simple identifier or t.col
                                            let clean = item.split('.').last().unwrap_or(item).trim().trim_matches(|c| c == '"' || c == '`');
                                            cols.push((clean.to_string(), inferred_type));
                                        }
                                    }
                                    Some(cols)
                                };

                                results.push((alias.to_string(), table_name.to_string(), specific_cols));
                            }
                        }
                    }
                }
            }
            pos = i;
        }
        results
    }

    /// Find keyword at parenthesis depth 0, ignoring content in quotes
    fn find_top_level_keyword(s: &str, keyword: &str) -> Option<usize> {
        let kw_len = keyword.len();
        let bytes = s.as_bytes();
        let len = bytes.len();
        let mut depth = 0;
        let mut in_quote = false;

        let mut i = 0;
        while i < len {
            let b = bytes[i];
            if in_quote {
                if b == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    in_quote = false;
                }
                i += 1;
                continue;
            }

            if b == b'\'' {
                in_quote = true;
                i += 1;
                continue;
            }

            if b == b'(' {
                depth += 1;
                i += 1;
                continue;
            }

            if b == b')' {
                if depth > 0 {
                    depth -= 1;
                }
                i += 1;
                continue;
            }

            if depth == 0 && i + kw_len <= len && s[i..i + kw_len].eq_ignore_ascii_case(keyword) {
                let prev_ok = i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
                let next_ok = i + kw_len == len || (!bytes[i + kw_len].is_ascii_alphanumeric() && bytes[i + kw_len] != b'_');
                if prev_ok && next_ok {
                    return Some(i);
                }
            }

            i += 1;
        }

        None
    }

    /// Split string by comma at parenthesis depth 0, ignoring commas in quotes
    fn split_top_level_comma(s: &str) -> Vec<String> {
        let bytes = s.as_bytes();
        let len = bytes.len();
        let mut depth = 0;
        let mut in_quote = false;
        let mut parts = Vec::new();
        let mut last = 0;

        let mut i = 0;
        while i < len {
            let b = bytes[i];
            if in_quote {
                if b == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    in_quote = false;
                }
                i += 1;
                continue;
            }

            if b == b'\'' {
                in_quote = true;
                i += 1;
                continue;
            }

            if b == b'(' {
                depth += 1;
                i += 1;
                continue;
            }

            if b == b')' {
                if depth > 0 {
                    depth -= 1;
                }
                i += 1;
                continue;
            }

            if depth == 0 && b == b',' {
                parts.push(s[last..i].trim().to_string());
                last = i + 1;
            }

            i += 1;
        }

        if last < len {
            let tail = s[last..].trim();
            if !tail.is_empty() {
                parts.push(tail.to_string());
            }
        }

        parts
    }

    /// Check if query contains json_populate_record or json_populate_recordset
    pub fn contains_json_populate_record_call(query: &str) -> bool {
        let q = query.to_lowercase();
        q.contains("json_populate_record(") || q.contains("json_populate_recordset(")
    }

    /// Check if query contains UPDATE <table> <alias> SET pattern (without AS)
    pub fn contains_update_table_alias(query: &str) -> bool {
        let q = query.to_uppercase();
        q.contains("UPDATE ") && q.contains(" SET")
    }

    /// Translate UPDATE <table> <alias> SET into UPDATE <table> AS <alias> SET
    pub fn translate_update_table_alias(query: &str) -> String {
        use regex::Regex;
        let re = match Regex::new(r"(?i)\bUPDATE\s+([a-zA-Z_][a-zA-Z0-9_]*)\s+([a-zA-Z_][a-zA-Z0-9_]*)\s+SET\b") {
            Ok(r) => r,
            Err(_) => return query.to_string(),
        };
        re.replace_all(query, |caps: &regex::Captures<'_>| {
            let tbl = &caps[1];
            let alias = &caps[2];
            if alias.eq_ignore_ascii_case("set") || alias.eq_ignore_ascii_case("as") {
                caps[0].to_string()
            } else {
                format!("UPDATE {tbl} AS {alias} SET")
            }
        }).to_string()
    }

    /// Translate json_populate_record(NULL::table, json) and json_populate_recordset(NULL::table, json)
    pub fn translate_json_populate_record_with_conn(
        query: &str,
        conn: &rusqlite::Connection,
    ) -> String {
        use regex::Regex;

        let call_pattern = match Regex::new(r"(?i)\b(json_populate_recordset|json_populate_record)\s*\(") {
            Ok(r) => r,
            Err(_) => return query.to_string(),
        };

        let mut result = query.to_string();

        while let Some(mat) = call_pattern.find(&result) {
            let fn_name = result[mat.start()..mat.end() - 1].trim().to_string();
            let is_recordset = fn_name.eq_ignore_ascii_case("json_populate_recordset");

            // Find matching closing paren
            let mut depth = 1;
            let mut in_quote = false;
            let mut j = mat.end();
            let bytes = result.as_bytes();
            while j < bytes.len() && depth > 0 {
                let b = bytes[j];
                if in_quote {
                    if b == b'\'' {
                        if j + 1 < bytes.len() && bytes[j + 1] == b'\'' {
                            j += 2;
                            continue;
                        }
                        in_quote = false;
                    }
                    j += 1;
                    continue;
                }
                if b == b'\'' {
                    in_quote = true;
                    j += 1;
                    continue;
                }
                if b == b'(' {
                    depth += 1;
                } else if b == b')' {
                    depth -= 1;
                }
                j += 1;
            }

            if depth != 0 {
                break;
            }

            let full_match = result[mat.start()..j].to_string();
            let args_str = &result[mat.end()..j - 1];
            let args = Self::split_top_level_comma(args_str);
            if args.len() < 2 {
                break;
            }

            // Parse arg1: table name from NULL::table or CAST(NULL AS table)
            let arg1 = args[0].trim();
            let raw_table = if let Some(idx) = arg1.rfind("::") {
                &arg1[idx + 2..]
            } else if let Some(as_pos) = arg1.to_lowercase().rfind(" as ") {
                &arg1[as_pos + 4..]
            } else {
                arg1
            };
            let parsed_table = raw_table
                .trim()
                .trim_matches(|c| c == '"' || c == '`' || c == '(' || c == ')')
                .trim();

            let table_name = if parsed_table.eq_ignore_ascii_case("null") || parsed_table.is_empty() {
                if let Some(inferred) = Self::extract_target_table(query) {
                    inferred
                } else {
                    break;
                }
            } else {
                parsed_table.to_string()
            };

            // Parse arg2: json expression
            let mut arg2 = args[1].trim().to_string();
            if args.len() > 2 {
                for extra in &args[2..] {
                    arg2.push(',');
                    arg2.push_str(extra);
                }
            }
            let clean_arg2 = if let Some(idx) = arg2.rfind("::") {
                let after = arg2[idx + 2..].trim();
                if after.eq_ignore_ascii_case("json")
                    || after.eq_ignore_ascii_case("jsonb")
                    || after.eq_ignore_ascii_case("text")
                {
                    arg2[..idx].trim().to_string()
                } else {
                    arg2
                }
            } else {
                arg2
            };

            // Query columns for table_name
            let pragma_sql = format!("PRAGMA table_info(\"{table_name}\")");
            let Ok(mut stmt) = conn.prepare(&pragma_sql) else {
                break;
            };
            let all_cols: Vec<(String, String)> = match stmt.query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            }) {
                Ok(mapped) => mapped.filter_map(Result::ok).collect(),
                Err(_) => break,
            };
            if all_cols.is_empty() {
                break;
            }

            // Fetch pg_type from __pgsqlite_schema if available
            let mut pg_types = std::collections::HashMap::new();
            if let Ok(mut schema_stmt) = conn.prepare(
                "SELECT column_name, pg_type FROM __pgsqlite_schema WHERE LOWER(table_name) = LOWER(?1)",
            ) && let Ok(rows) = schema_stmt.query_map([table_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }) {
                for (c, t) in rows.flatten() {
                    pg_types.insert(c.to_lowercase(), t.to_uppercase());
                }
            }

            let mut fields = Vec::with_capacity(all_cols.len());
            for (col_name, sqlite_type) in &all_cols {
                let src = if is_recordset { "j.value" } else { &clean_arg2 };
                let eff_type = pg_types
                    .get(&col_name.to_lowercase())
                    .cloned()
                    .unwrap_or_else(|| sqlite_type.to_uppercase());

                let raw_extract = format!("json_extract({src}, '$.{col_name}')");
                let expr = if eff_type.contains("TIMESTAMP") || col_name.ends_with("_at") {
                    format!("pgsqlite_parse_timestamp({raw_extract})")
                } else if eff_type == "BOOL" || eff_type == "BOOLEAN" {
                    format!("CASE WHEN {raw_extract} IN (1, '1', 't', 'true') THEN 1 WHEN {raw_extract} IS NULL THEN NULL ELSE 0 END")
                } else {
                    raw_extract
                };
                fields.push(format!("{expr} AS {col_name}"));
            }

            let replacement = if is_recordset {
                format!("(SELECT {} FROM json_each({}) AS j)", fields.join(", "), clean_arg2)
            } else {
                format!("(SELECT {})", fields.join(", "))
            };

            result = result.replacen(&full_match, &replacement, 1);
        }

        Self::fix_insert_select_on_conflict(&result)
    }

    /// Disambiguate INSERT ... SELECT ... FROM ... ON CONFLICT for SQLite.
    /// SQLite's parser incorrectly treats ON CONFLICT as a JOIN condition on the FROM item
    /// unless terminated by WHERE, GROUP BY, HAVING, WINDOW, ORDER BY, or LIMIT.
    pub fn fix_insert_select_on_conflict(sql: &str) -> String {
        let sql_upper = sql.to_uppercase();
        if !sql_upper.contains("INSERT")
            || !sql_upper.contains("SELECT")
            || !sql_upper.contains("FROM")
            || !sql_upper.contains("ON CONFLICT")
        {
            return sql.to_string();
        }

        let bytes = sql.as_bytes();
        let len = sql.len();
        let mut depth = 0;
        let mut in_quote = false;
        let mut quote_char = b' ';
        let mut on_conflict_pos = None;
        let mut i = 0;

        while i < len {
            let b = bytes[i];
            if in_quote {
                if b == quote_char {
                    if i + 1 < len && bytes[i + 1] == quote_char {
                        i += 2;
                        continue;
                    }
                    in_quote = false;
                }
            } else if b == b'\'' || b == b'"' {
                in_quote = true;
                quote_char = b;
            } else if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth -= 1;
            } else if depth == 0 && i + 11 <= len && bytes[i..i + 11].eq_ignore_ascii_case(b"on conflict") {
                let before_ok = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                let after_ok = i + 11 == len || !(bytes[i + 11].is_ascii_alphanumeric() || bytes[i + 11] == b'_');
                if before_ok && after_ok && sql.is_char_boundary(i) {
                    on_conflict_pos = Some(i);
                    break;
                }
            }
            i += 1;
        }

        let Some(pos) = on_conflict_pos else {
            return sql.to_string();
        };

        // Collect depth-0 tokens before on_conflict_pos
        let before = &sql[..pos];
        let before_bytes = before.as_bytes();
        let mut tokens: Vec<String> = Vec::new();
        let mut curr = String::new();
        let mut depth = 0;
        let mut in_quote = false;
        let mut quote_char = b' ';
        let mut idx = 0;

        while idx < before.len() {
            let b = before_bytes[idx];
            if in_quote {
                if b == quote_char {
                    if idx + 1 < before.len() && before_bytes[idx + 1] == quote_char {
                        idx += 2;
                        continue;
                    }
                    in_quote = false;
                }
            } else if b == b'\'' || b == b'"' {
                in_quote = true;
                quote_char = b;
            } else if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth -= 1;
            } else if depth == 0 {
                if b.is_ascii_alphanumeric() || b == b'_' {
                    curr.push((b as char).to_ascii_uppercase());
                } else if !curr.is_empty() {
                    tokens.push(std::mem::take(&mut curr));
                }
            }
            idx += 1;
        }
        if !curr.is_empty() {
            tokens.push(curr);
        }

        if let Some(from_idx) = tokens.iter().rposition(|t| t == "FROM") {
            let after_from = &tokens[from_idx + 1..];
            let has_terminator = after_from.iter().any(|t| {
                matches!(
                    t.as_str(),
                    "WHERE" | "GROUP" | "HAVING" | "WINDOW" | "ORDER" | "LIMIT"
                )
            });
            if !has_terminator {
                return format!("{}WHERE 1=1 {}", &sql[..pos], &sql[pos..]);
            }
        }

        sql.to_string()
    }

    /// Extract target table from INSERT INTO <table> or UPDATE <table>
    fn extract_target_table(query: &str) -> Option<String> {
        use regex::Regex;
        if let Ok(re) = Regex::new(r"(?i)\b(?:INSERT\s+INTO|UPDATE)\s+([a-zA-Z_][a-zA-Z0-9_]*)")
            && let Some(caps) = re.captures(query)
        {
            return Some(caps[1].to_string());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_contains_row_to_json_call() {
        assert!(RowToJsonTranslator::contains_row_to_json_call("SELECT row_to_json(t) FROM users t"));
        assert!(RowToJsonTranslator::contains_row_to_json_call("SELECT ROW_TO_JSON(t) FROM users t"));
        assert!(!RowToJsonTranslator::contains_row_to_json_call("SELECT * FROM users"));
    }
    
    #[test]
    fn test_extract_columns_from_select() {
        let columns = RowToJsonTranslator::extract_columns_from_select("SELECT name, age FROM users");
        assert_eq!(columns, Some(vec!["name".to_string(), "age".to_string()]));
        
        let columns = RowToJsonTranslator::extract_columns_from_select("SELECT name AS user_name, age FROM users");
        assert_eq!(columns, Some(vec!["user_name".to_string(), "age".to_string()]));
    }
    
    #[test]
    fn test_translate_subquery_pattern() {
        let query = "SELECT row_to_json(t) FROM (SELECT name, age FROM users WHERE id = 1) t";
        let mut metadata = TranslationMetadata::new();
        
        let result = RowToJsonTranslator::translate_subquery_pattern(query, &mut metadata);
        assert!(result.is_some(), "Expected translation result for query: {query}");
        
        let translated = result.unwrap();
        assert!(translated.contains("json_object('name', name, 'age', age)"));
    }

    #[test]
    fn test_translate_subquery_with_conn() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE tasks (id INTEGER PRIMARY KEY, title TEXT, status TEXT)", []).unwrap();
        let query = "SELECT CAST(row_to_json(row_data) AS TEXT) FROM (SELECT * FROM tasks WHERE id = 10) row_data";
        let (translated, _) = RowToJsonTranslator::translate_row_to_json_with_conn(query, Some(&conn));
        assert!(translated.contains("json_object("), "Result should contain json_object: {translated}");
        assert!(translated.contains("'id', row_data.id"), "Result should contain id field: {translated}");
        assert!(translated.contains("'title', row_data.title"), "Result should contain title field: {translated}");
        assert!(translated.contains("'status', row_data.status"), "Result should contain status field: {translated}");
        assert!(!translated.contains("CAST("), "Outer CAST should be consumed: {translated}");

        let query2 = "SELECT row_to_json(row_data)::text FROM (SELECT * FROM tasks WHERE id = 10) row_data";
        let (translated2, _) = RowToJsonTranslator::translate_row_to_json_with_conn(query2, Some(&conn));
        assert!(translated2.contains("json_object("), "Result should contain json_object: {translated2}");
        assert!(!translated2.contains("::text"), "Trailing cast should be consumed: {translated2}");
    }

    #[test]
    fn test_translate_subquery_with_expressions() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE llm_usage (id INTEGER PRIMARY KEY, profile_name TEXT, input_tokens INTEGER)",
            [],
        )
        .unwrap();
        let query = "SELECT COALESCE(jsonb_agg(to_jsonb(x)), '[]') FROM (SELECT COALESCE(profile_name, '') AS profile_name, count(*) AS calls, 1 AS tasks FROM llm_usage GROUP BY profile_name) x";
        let (translated, _) = RowToJsonTranslator::translate_row_to_json_with_conn(query, Some(&conn));
        assert!(translated.contains("json_object("), "Result should contain json_object: {translated}");
        assert!(translated.contains("'profile_name', x.profile_name"), "Should contain profile_name: {translated}");
        assert!(translated.contains("'calls', x.calls"), "Should contain calls: {translated}");
        assert!(translated.contains("'tasks', x.tasks"), "Should contain tasks: {translated}");
        assert!(!translated.contains("x.id"), "Should not contain x.id: {translated}");
    }

    #[test]
    fn test_translate_json_populate_recordset() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE finding_traffic_bindings (id INTEGER PRIMARY KEY, finding_id INTEGER, snapshot_id TEXT)",
            [],
        )
        .unwrap();
        let query = "INSERT INTO finding_traffic_bindings SELECT * FROM json_populate_recordset(NULL::finding_traffic_bindings,$1::json)";
        let (translated, _) = RowToJsonTranslator::translate_row_to_json_with_conn(query, Some(&conn));
        assert!(translated.contains("json_each($1)"), "Result should use json_each: {translated}");
        assert!(translated.contains("json_extract(j.value, '$.id') AS id"), "Should extract id: {translated}");
        assert!(translated.contains("json_extract(j.value, '$.finding_id') AS finding_id"), "Should extract finding_id: {translated}");
    }

    #[test]
    fn test_translate_json_populate_record_and_update_alias() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE explorations (id INTEGER PRIMARY KEY, description TEXT, goal TEXT)",
            [],
        )
        .unwrap();
        let query = "UPDATE explorations current SET description=archived.description FROM json_populate_record(NULL::explorations,$2::json) archived WHERE current.id=$1 AND archived.id=$1";
        let (translated, _) = RowToJsonTranslator::translate_row_to_json_with_conn(query, Some(&conn));
        assert!(translated.contains("UPDATE explorations AS current SET"), "Should add AS: {translated}");
        assert!(translated.contains("FROM (SELECT json_extract($2, '$.id') AS id"), "Should translate record: {translated}");
    }

    #[test]
    fn test_fix_insert_select_on_conflict() {
        let q1 = "INSERT INTO assets SELECT * FROM (SELECT 1 AS id) ON CONFLICT DO NOTHING";
        assert_eq!(
            RowToJsonTranslator::fix_insert_select_on_conflict(q1),
            "INSERT INTO assets SELECT * FROM (SELECT 1 AS id) WHERE 1=1 ON CONFLICT DO NOTHING"
        );
        let q2 = "INSERT INTO assets SELECT * FROM (SELECT 1 AS id) WHERE id = 1 ON CONFLICT DO NOTHING";
        assert_eq!(RowToJsonTranslator::fix_insert_select_on_conflict(q2), q2);

        let q3 = "INSERT INTO exploration_anchors(node_id, asset_id) VALUES ($1,$2) ON CONFLICT DO NOTHING";
        assert_eq!(RowToJsonTranslator::fix_insert_select_on_conflict(q3), q3);

        let q4 = "INSERT INTO tasks (name, note) VALUES ('执行任务', '说明') ON CONFLICT DO NOTHING";
        assert_eq!(RowToJsonTranslator::fix_insert_select_on_conflict(q4), q4);

        let q5 = "INSERT INTO assets SELECT * FROM (SELECT '执行' AS name) ON CONFLICT DO NOTHING";
        assert_eq!(
            RowToJsonTranslator::fix_insert_select_on_conflict(q5),
            "INSERT INTO assets SELECT * FROM (SELECT '执行' AS name) WHERE 1=1 ON CONFLICT DO NOTHING"
        );
    }

    #[test]
    fn test_row_to_json_array_column() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE assets (id INTEGER PRIMARY KEY, task_ids TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE __pgsqlite_schema (table_name TEXT, column_name TEXT, pg_type TEXT, sqlite_type TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO __pgsqlite_schema VALUES ('assets', 'task_ids', 'BIGINT[]', 'TEXT')",
            [],
        )
        .unwrap();

        let query = "SELECT CAST(row_to_json(row_data) AS TEXT) FROM (SELECT * FROM assets) row_data";
        let (translated, _) = RowToJsonTranslator::translate_row_to_json_with_conn(query, Some(&conn));
        assert!(
            translated.contains("json(array_to_json(row_data.task_ids))"),
            "Array column must wrap in json(array_to_json(...)): {translated}"
        );
    }
}