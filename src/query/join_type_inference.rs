use std::collections::HashMap;
use regex::Regex;
use once_cell::sync::Lazy;
use tracing::info;

// Regex to extract all tables from a JOIN query
static JOIN_TABLES_PATTERN: Lazy<Regex> = Lazy::new(|| {
    // Match FROM/JOIN followed by table name
    // Stop before ON/WHERE/AS keywords
    Regex::new(r"(?i)(?:FROM|JOIN)\s+([^\s,()]+)").unwrap()
});

// Regex to extract column with table prefix (e.g., order_items.unit_price)
static COLUMN_WITH_TABLE_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\w+)\.(\w+)").unwrap()
});

/// Extract all table names and their aliases from a query
pub fn extract_table_alias_mapping(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();

    if let Ok(statements) = sqlparser::parser::Parser::parse_sql(
        &sqlparser::dialect::PostgreSqlDialect {},
        query,
    ) {
        for stmt in &statements {
            extract_aliases_from_statement(stmt, &mut map);
        }
    }

    extract_aliases_regex(query, &mut map);
    map
}

fn extract_aliases_from_statement(stmt: &sqlparser::ast::Statement, map: &mut HashMap<String, String>) {
    use sqlparser::ast::*;
    match stmt {
        Statement::Query(q) => extract_aliases_from_query(q, map),
        Statement::Update(update) => {
            if let TableFactor::Table { name, alias, .. } = &update.table.relation {
                let real_name = name.to_string();
                map.insert(real_name.to_lowercase(), real_name.clone());
                if let Some(a) = alias {
                    map.insert(a.name.value.to_lowercase(), real_name);
                }
            }
        }
        Statement::Delete(delete) => {
            match &delete.from {
                FromTable::WithFromKeyword(tables) => {
                    for t in tables {
                        if let TableFactor::Table { name, alias, .. } = &t.relation {
                            let real_name = name.to_string();
                            map.insert(real_name.to_lowercase(), real_name.clone());
                            if let Some(a) = alias {
                                map.insert(a.name.value.to_lowercase(), real_name);
                            }
                        }
                    }
                }
                FromTable::WithoutKeyword(names) => {
                    for name in names {
                        let real_name = name.to_string();
                        map.insert(real_name.to_lowercase(), real_name);
                    }
                }
            }
        }
        _ => {}
    }
}

fn extract_aliases_from_query(query: &sqlparser::ast::Query, map: &mut HashMap<String, String>) {
    use sqlparser::ast::*;
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            extract_aliases_from_query(&cte.query, map);
        }
    }
    match query.body.as_ref() {
        SetExpr::Select(select) => {
            for table in &select.from {
                extract_aliases_from_table_factor(&table.relation, map);
                for join in &table.joins {
                    extract_aliases_from_table_factor(&join.relation, map);
                }
            }
        }
        SetExpr::Query(q) => extract_aliases_from_query(q, map),
        SetExpr::SetOperation { left, right, .. } => {
            if let SetExpr::Select(select) = left.as_ref() {
                for table in &select.from {
                    extract_aliases_from_table_factor(&table.relation, map);
                    for join in &table.joins {
                        extract_aliases_from_table_factor(&join.relation, map);
                    }
                }
            }
            if let SetExpr::Select(select) = right.as_ref() {
                for table in &select.from {
                    extract_aliases_from_table_factor(&table.relation, map);
                    for join in &table.joins {
                        extract_aliases_from_table_factor(&join.relation, map);
                    }
                }
            }
        }
        _ => {}
    }
}

fn extract_aliases_from_table_factor(factor: &sqlparser::ast::TableFactor, map: &mut HashMap<String, String>) {
    use sqlparser::ast::TableFactor;
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let real_name = name.to_string();
            map.insert(real_name.to_lowercase(), real_name.clone());
            if let Some(a) = alias {
                map.insert(a.name.value.to_lowercase(), real_name);
            }
        }
        TableFactor::Derived { subquery, alias, .. } => {
            extract_aliases_from_query(subquery, map);
            if let Some(a) = alias {
                if let sqlparser::ast::SetExpr::Select(select) = subquery.body.as_ref() {
                    if let Some(first_table) = select.from.first() {
                        if let TableFactor::Table { name, .. } = &first_table.relation {
                            map.insert(a.name.value.to_lowercase(), name.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn extract_aliases_regex(query: &str, map: &mut HashMap<String, String>) {
    static TABLE_JOIN_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\b(?:FROM|(?:LEFT\s+|RIGHT\s+|INNER\s+|CROSS\s+|FULL\s+)?(?:OUTER\s+)?JOIN)\s+([a-zA-Z0-9_]+)(?:\s+(?:AS\s+)?([a-zA-Z0-9_]+))?").unwrap()
    });
    const SQL_KEYWORDS: &[&str] = &[
        "on", "where", "join", "left", "right", "inner", "cross", "full", "outer",
        "group", "order", "having", "limit", "window", "union", "except", "intersect",
        "natural", "using", "set", "as",
    ];

    for cap in TABLE_JOIN_RE.captures_iter(query) {
        if let Some(tbl_match) = cap.get(1) {
            let tbl_name = tbl_match.as_str().trim_matches('"').trim_matches('\'');
            if !SQL_KEYWORDS.contains(&tbl_name.to_lowercase().as_str()) {
                map.entry(tbl_name.to_lowercase()).or_insert_with(|| tbl_name.to_string());
                if let Some(alias_match) = cap.get(2) {
                    let alias = alias_match.as_str().trim_matches('"').trim_matches('\'');
                    if !SQL_KEYWORDS.contains(&alias.to_lowercase().as_str()) {
                        map.insert(alias.to_lowercase(), tbl_name.to_string());
                    }
                }
            }
        }
    }
}

/// Extract all table names from a query with JOINs
pub fn extract_all_tables_from_query(query: &str) -> Vec<(String, Option<String>)> {
    let mut tables = Vec::new();
    
    for cap in JOIN_TABLES_PATTERN.captures_iter(query) {
        let table_name = cap[1].trim_matches('"').trim_matches('\'').to_string();
        
        // Skip if it's a subquery or function
        if !table_name.starts_with('(') && !table_name.contains('(') {
            // For now, we don't capture aliases from this regex
            tables.push((table_name, None));
        }
    }
    
    tables
}

/// Extract the table name for a specific column from the query
pub fn extract_table_for_column(query: &str, column_name: &str) -> Option<String> {
    let alias_map = extract_table_alias_mapping(query);

    // First check if the column name already includes the table prefix
    if let Some(caps) = COLUMN_WITH_TABLE_PATTERN.captures(column_name) {
        let prefix = &caps[1];
        return Some(alias_map.get(&prefix.to_lowercase()).cloned().unwrap_or_else(|| prefix.to_string()));
    }
    
    // Look for explicit table.column pattern in the query
    let pattern = format!(r"(?i)(\w+)\.{}\b", regex::escape(column_name));
    if let Ok(re) = Regex::new(&pattern)
        && let Some(caps) = re.captures(query) {
            let prefix = &caps[1];
            return Some(alias_map.get(&prefix.to_lowercase()).cloned().unwrap_or_else(|| prefix.to_string()));
        }
    
    // If not found, check aliases
    let alias_pattern = format!(r"(?i)(\w+)\.(\w+)\s+AS\s+{}\b", regex::escape(column_name));
    if let Ok(re) = Regex::new(&alias_pattern)
        && let Some(caps) = re.captures(query) {
            let prefix = &caps[1];
            return Some(alias_map.get(&prefix.to_lowercase()).cloned().unwrap_or_else(|| prefix.to_string()));
        }
    
    None
}

/// Extract the (real_table, source_column) for a column in a SELECT query
pub fn extract_source_table_column(
    query: &str,
    col_name: &str,
    col_idx: Option<usize>,
) -> Option<(String, String)> {
    let alias_map = extract_table_alias_mapping(query);

    // 1. If AST is available and col_idx is provided, check projection item directly
    if let Some(idx) = col_idx {
        if let Ok(statements) = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::PostgreSqlDialect {},
            query,
        ) {
            if let Some(sqlparser::ast::Statement::Query(q)) = statements.first() {
                if let sqlparser::ast::SetExpr::Select(select) = q.body.as_ref() {
                    let has_wildcard = select.projection.iter().any(|item| {
                        matches!(item, sqlparser::ast::SelectItem::Wildcard(_) | sqlparser::ast::SelectItem::QualifiedWildcard(..))
                    });
                    if !has_wildcard && idx < select.projection.len() {
                        let item = &select.projection[idx];
                        if let Some((tbl, col)) = resolve_projection_item(item, &alias_map) {
                            return Some((tbl, col));
                        }
                    }
                }
            }
        }
    }

    // 2. Check if query has scalar subquery `(SELECT <col> FROM <tbl> ...) AS <col_name>`
    let subquery_as_pattern = format!(r#"(?is)\(\s*SELECT\s+([a-zA-Z0-9_]+)\s+FROM\s+([a-zA-Z0-9_]+)\b[^\)]*\)\s+AS\s+{}\b"#, regex::escape(col_name));
    if let Ok(re) = Regex::new(&subquery_as_pattern) {
        if let Some(caps) = re.captures(query) {
            let col = caps[1].to_string();
            let tbl = caps[2].to_string();
            return Some((tbl, col));
        }
    }

    // 3. Check if query has `<expr> AS <col_name>` (case-insensitive)
    let as_pattern = format!(r"(?i)\s+AS\s+{}\b", regex::escape(col_name));
    if let Ok(re) = Regex::new(&as_pattern) {
        if let Some(m) = re.find(query) {
            let before_as = &query[..m.start()].trim_end();
            let start = before_as.rfind(|c| c == ',' || c == '(').map(|p| p + 1).unwrap_or(0);
            let expr = before_as[start..].trim();
            if let Some(dot_pos) = expr.rfind('.') {
                let prefix = expr[..dot_pos].trim().trim_start_matches('(').trim();
                let actual_col = expr[dot_pos + 1..].trim();
                if prefix.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && actual_col.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    let real_tbl = alias_map.get(&prefix.to_lowercase()).cloned().unwrap_or_else(|| prefix.to_string());
                    return Some((real_tbl, actual_col.to_string()));
                }
            } else if expr.chars().all(|c| c.is_alphanumeric() || c == '_') {
                if let Some(tbl) = alias_map.values().next() {
                    return Some((tbl.clone(), expr.to_string()));
                }
            }
        }
    }

    // 3. Check for `<prefix>.<col_name>\b` in query
    let dot_pattern = format!(r"(?i)\b([a-zA-Z0-9_]+)\.{}\b", regex::escape(col_name));
    if let Ok(re) = Regex::new(&dot_pattern) {
        if let Some(caps) = re.captures(query) {
            let prefix = &caps[1];
            let real_tbl = alias_map.get(&prefix.to_lowercase()).cloned().unwrap_or_else(|| prefix.to_string());
            return Some((real_tbl, col_name.to_string()));
        }
    }

    // 4. If col_name itself contains an expression with a dot (e.g. "COALESCE(p.last_error,'')")
    let col_lower = col_name.to_ascii_lowercase();
    if !col_lower.contains("json_object") && !col_lower.contains("case when") {
        if let Some(caps) = COLUMN_WITH_TABLE_PATTERN.captures(col_name) {
            let prefix = &caps[1];
            let actual_col = &caps[2];
            let real_tbl = alias_map.get(&prefix.to_lowercase()).cloned().unwrap_or_else(|| prefix.to_string());
            return Some((real_tbl, actual_col.to_string()));
        }
    }

    None
}

fn resolve_projection_item(
    item: &sqlparser::ast::SelectItem,
    alias_map: &HashMap<String, String>,
) -> Option<(String, String)> {
    use sqlparser::ast::*;
    match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            resolve_expr_source(expr, alias_map)
        }
        _ => None,
    }
}

fn resolve_expr_source(
    expr: &sqlparser::ast::Expr,
    alias_map: &HashMap<String, String>,
) -> Option<(String, String)> {
    use sqlparser::ast::*;
    match expr {
        Expr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                let prefix = &parts[parts.len() - 2].value;
                let col = &parts[parts.len() - 1].value;
                let real_tbl = alias_map.get(&prefix.to_lowercase()).cloned().unwrap_or_else(|| prefix.to_string());
                Some((real_tbl, col.to_string()))
            } else {
                None
            }
        }
        Expr::Identifier(ident) => {
            let col = &ident.value;
            if alias_map.len() == 1 {
                Some((alias_map.values().next().unwrap().clone(), col.to_string()))
            } else {
                None
            }
        }
        Expr::Function(func) => {
            if let FunctionArguments::List(arg_list) = &func.args {
                for arg in &arg_list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                        if let Some(res) = resolve_expr_source(e, alias_map) {
                            return Some(res);
                        }
                    }
                }
            }
            None
        }
        Expr::Nested(e) => resolve_expr_source(e, alias_map),
        _ => None,
    }
}

/// Build a mapping of column names to their source tables for a JOIN query
pub fn build_column_to_table_mapping(query: &str) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    let alias_map = extract_table_alias_mapping(query);

    // Extract all table.column patterns
    let column_pattern = Regex::new(r"(?i)(\w+)\.(\w+)(?:\s+AS\s+(\w+))?").unwrap();

    for cap in column_pattern.captures_iter(query) {
        let prefix = cap[1].to_string();
        let column = cap[2].to_string();
        let table = alias_map.get(&prefix.to_lowercase()).cloned().unwrap_or(prefix);

        // Add mapping for the original column name
        mapping.insert(column.clone(), table.clone());

        // If there's an alias, add mapping for that too
        if let Some(alias) = cap.get(3) {
            let alias_str = alias.as_str();
            mapping.insert(alias_str.to_string(), table.clone());
            info!("JOIN mapping: alias '{}' -> table '{}'", alias_str, table);

            // Also add a special mapping for SQLAlchemy patterns like "order_items_unit_price"
            // where the alias is "table_column" format
            if alias_str == format!("{table}_{column}") {
                mapping.insert(alias_str.to_string(), table.clone());
                info!("JOIN mapping: SQLAlchemy pattern detected '{}' for {}.{}", alias_str, table, column);
            }
        }

        // Also add table.column format
        mapping.insert(format!("{table}.{column}"), table.clone());
    }

    info!("JOIN mapping: Built {} mappings from query", mapping.len());

    mapping
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_all_tables() {
        let query = "SELECT * FROM orders JOIN users ON orders.customer_id = users.id JOIN order_items ON orders.id = order_items.order_id";
        let tables = extract_all_tables_from_query(query);

        assert_eq!(tables.len(), 3);
        assert_eq!(tables[0].0, "orders");
        assert_eq!(tables[1].0, "users");
        assert_eq!(tables[2].0, "order_items");
    }

    #[test]
    fn test_column_to_table_mapping() {
        let query = "SELECT orders.id AS orders_id, users.username AS users_username, order_items.unit_price AS order_items_unit_price FROM orders JOIN users ON orders.customer_id = users.id JOIN order_items ON orders.id = order_items.order_id";
        let mapping = build_column_to_table_mapping(query);

        assert_eq!(mapping.get("orders_id"), Some(&"orders".to_string()));
        assert_eq!(mapping.get("users_username"), Some(&"users".to_string()));
        assert_eq!(mapping.get("order_items_unit_price"), Some(&"order_items".to_string()));
        assert_eq!(mapping.get("unit_price"), Some(&"order_items".to_string()));
    }

    #[test]
    fn test_extract_source_table_column_with_aliases() {
        let query = "SELECT t.llm_profile_id, t.active_llm_profile_id, t.llm_chain_revision, p.profile_id, p.position, p.status, COALESCE(p.last_error,''), p.exhausted_at FROM tasks t LEFT JOIN task_llm_profiles p ON p.task_id=t.id WHERE t.id=$1 ORDER BY p.position NULLS LAST";

        let res0 = extract_source_table_column(query, "llm_profile_id", Some(0));
        assert_eq!(res0, Some(("tasks".to_string(), "llm_profile_id".to_string())));

        let res7 = extract_source_table_column(query, "exhausted_at", Some(7));
        assert_eq!(res7, Some(("task_llm_profiles".to_string(), "exhausted_at".to_string())));

        let res_no_idx = extract_source_table_column(query, "exhausted_at", None);
        assert_eq!(res_no_idx, Some(("task_llm_profiles".to_string(), "exhausted_at".to_string())));
    }
}
