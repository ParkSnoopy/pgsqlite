use once_cell::sync::Lazy;
use regex::Regex;

/// Translates PostgreSQL data-modifying Common Table Expressions (DML inside WITH)
/// into SQLite-compatible DML statements with RETURNING clauses.
pub struct DmlCteTranslator;

// Pattern 1: Single DML CTE
// e.g. WITH inserted AS ( INSERT INTO ... RETURNING * ) SELECT ... FROM inserted
static SINGLE_DML_CTE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?is)^\s*WITH\s+([a-zA-Z0-9_]+)\s+AS\s*\(\s*(INSERT\s+INTO\s+([^\s(]+)|UPDATE\s+([^\s]+)|DELETE\s+FROM\s+([^\s]+))([\s\S]+?)\s+RETURNING\s+[\s\S]+?\)\s*SELECT\s+([\s\S]+?)\s+FROM\s+([a-zA-Z0-9_]+)\s*;?\s*$").unwrap()
});

// Pattern 2: Read-only CTE followed by DML CTE
// e.g. WITH selected AS ( SELECT * FROM t WHERE id=$2 ), updated AS ( UPDATE ... ) SELECT ... FROM selected, updated
static TWO_CTE_DML_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?is)^\s*WITH\s+([a-zA-Z0-9_]+)\s+AS\s*\(\s*SELECT\s+\*\s+FROM\s+([a-zA-Z0-9_]+)\s+WHERE\s+([^)]+)\s*\)\s*,\s*([a-zA-Z0-9_]+)\s+AS\s*\(\s*(UPDATE\s+[^\s]+[\s\S]+?)\s+RETURNING\s+[\s\S]+?\)\s*SELECT\s+([\s\S]+?)\s+FROM\s+([a-zA-Z0-9_]+)\s*,\s*([a-zA-Z0-9_]+)\s*;?\s*$").unwrap()
});

impl DmlCteTranslator {
    /// Check if query contains a data-modifying CTE.
    pub fn is_dml_cte(query: &str) -> bool {
        let trimmed = query.trim();
        if !trimmed.to_ascii_uppercase().starts_with("WITH") {
            return false;
        }
        let upper = trimmed.to_ascii_uppercase();
        upper.contains("INSERT INTO") || upper.contains("UPDATE ") || upper.contains("DELETE FROM")
    }

    /// Translate a data-modifying CTE query into an equivalent SQLite DML with RETURNING.
    pub fn translate(query: &str) -> Option<String> {
        let trimmed = query.trim();
        if !Self::is_dml_cte(trimmed) {
            return None;
        }

        // Try single DML CTE first
        if let Some(caps) = SINGLE_DML_CTE_REGEX.captures(trimmed) {
            let cte_name = caps.get(1)?.as_str();
            let from_table = caps.get(8)?.as_str();
            if cte_name.eq_ignore_ascii_case(from_table) {
                let dml_kw = caps.get(2)?.as_str();
                let table = caps.get(3).or_else(|| caps.get(4)).or_else(|| caps.get(5))?.as_str();
                let dml_body = caps.get(6)?.as_str();
                let outer_select = caps.get(7)?.as_str().trim();

                let dml_prefix = format!("{dml_kw}{dml_body}").trim().to_string();

                // Replace `<cte_name>.` with `<table_name>.` in outer select
                let cte_dot = format!("{cte_name}.");
                let clean_table = table.trim_matches('"');
                let table_dot = format!("{clean_table}.");
                let rewritten_cols = outer_select.replace(&cte_dot, &table_dot);

                return Some(format!("{dml_prefix} RETURNING {rewritten_cols}"));
            }
        }

        // Try two CTEs (read-only CTE + UPDATE CTE)
        if let Some(caps) = TWO_CTE_DML_REGEX.captures(trimmed) {
            let sel_name = caps.get(1)?.as_str();
            let sel_tbl = caps.get(2)?.as_str();
            let sel_where = caps.get(3)?.as_str().trim();
            let upd_name = caps.get(4)?.as_str();
            let from_1 = caps.get(7)?.as_str();
            let from_2 = caps.get(8)?.as_str();

            if sel_name.eq_ignore_ascii_case(from_1) && upd_name.eq_ignore_ascii_case(from_2) {
                let mut upd_stmt = caps.get(5)?.as_str().trim().to_string();
                let outer_cols = caps.get(6)?.as_str().trim();

                // Replace references to `FROM <sel_name>` in `upd_stmt` with `FROM <sel_tbl> WHERE <sel_where>`
                let sel_from = format!("FROM {sel_name}");
                let inlined_from = format!("FROM {sel_tbl} WHERE {sel_where}");
                upd_stmt = upd_stmt.replace(&sel_from, &inlined_from);

                // Split outer_cols by top-level commas (respecting parentheses)
                let mut top_level_items = Vec::new();
                let mut current = String::new();
                let mut depth = 0;
                for ch in outer_cols.chars() {
                    match ch {
                        '(' => {
                            depth += 1;
                            current.push(ch);
                        }
                        ')' => {
                            if depth > 0 {
                                depth -= 1;
                            }
                            current.push(ch);
                        }
                        ',' if depth == 0 => {
                            top_level_items.push(current.trim().to_string());
                            current = String::new();
                        }
                        _ => current.push(ch),
                    }
                }
                if !current.trim().is_empty() {
                    top_level_items.push(current.trim().to_string());
                }

                let col_ref_regex = Regex::new(&format!(r"\b{}\.([a-zA-Z0-9_]+)\b", regex::escape(sel_name))).ok()?;
                let mut rewritten_items = Vec::new();
                for item in top_level_items {
                    if let Some(c) = col_ref_regex.captures(&item) {
                        if c.get(0).map(|m| m.as_str()) == Some(item.as_str()) {
                            let col = &c[1];
                            rewritten_items.push(format!("(SELECT {col} FROM {sel_tbl} WHERE {sel_where}) AS {col}"));
                            continue;
                        }
                    }
                    let rewritten = col_ref_regex.replace_all(&item, |c: &regex::Captures<'_>| {
                        let col = &c[1];
                        format!("(SELECT {col} FROM {sel_tbl} WHERE {sel_where})")
                    }).to_string();
                    rewritten_items.push(rewritten);
                }
                let new_cols = rewritten_items.join(", ");

                return Some(format!("{upd_stmt} RETURNING {new_cols}"));
            }
        }

        // Try read-only CTE + INSERT CTE:
        // e.g. WITH requested(...) AS ( ... ), inserted AS ( INSERT INTO task_scope(...) ... RETURNING ... ) SELECT count(*) FROM inserted
        static CTE_INSERT_COUNT_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r#"(?is)^\s*WITH\s+([\s\S]+?),\s*([a-zA-Z0-9_]+)\s+AS\s*\(\s*(INSERT\s+INTO\s+[\s\S]+?)\s+RETURNING\s+[\s\S]+?\)\s*SELECT\s+count\(\*\)\s+FROM\s+([a-zA-Z0-9_]+)\s*;?\s*$"#).unwrap()
        });
        if let Some(caps) = CTE_INSERT_COUNT_RE.captures(trimmed) {
            let cte1 = caps.get(1)?.as_str().trim();
            let ins_name = caps.get(2)?.as_str();
            let ins_stmt = caps.get(3)?.as_str().trim();
            let from_tbl = caps.get(4)?.as_str();

            if ins_name.eq_ignore_ascii_case(from_tbl) {
                let from_idx = ins_stmt.to_ascii_uppercase().find("FROM ")?;
                let mut from_part = ins_stmt[from_idx + 5..].trim();
                if let Some(order_idx) = from_part.to_ascii_uppercase().find("ORDER BY ") {
                    from_part = from_part[..order_idx].trim();
                }
                return Some(format!(
                    "WITH {cte1}\n{ins_stmt}\nRETURNING (SELECT count(*) FROM {from_part}) AS count"
                ));
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_translate_insert_cte() {
        let sql = r#"
WITH inserted AS (
    INSERT INTO task_categories(name, nkey)
    VALUES ($1,$2)
    ON CONFLICT (nkey) DO NOTHING
    RETURNING *
)
SELECT inserted.id, inserted.name, inserted.nkey, 0, inserted.created_at, inserted.updated_at
FROM inserted"#;

        let translated = DmlCteTranslator::translate(sql).expect("translation failed");
        assert!(translated.starts_with("INSERT INTO task_categories"));
        assert!(translated.contains("RETURNING task_categories.id, task_categories.name"));
        assert!(translated.contains("task_categories.created_at"));
    }

    #[test]
    fn test_translate_update_cte() {
        let sql = r#"
WITH updated AS (
    UPDATE task_categories SET name=$2, nkey=$3 WHERE id=$1 RETURNING *
)
SELECT updated.id, updated.name, updated.nkey,
       (SELECT count(*) FROM tasks WHERE category_id=updated.id AND deleted_at IS NULL),
       updated.created_at, updated.updated_at
FROM updated"#;

        let translated = DmlCteTranslator::translate(sql).expect("translation failed");
        assert!(translated.starts_with("UPDATE task_categories SET name=$2"));
        assert!(translated.contains("RETURNING task_categories.id, task_categories.name"));
        assert!(translated.contains("task_categories.created_at"));
    }

    #[test]
    fn test_translate_two_cte_update() {
        let sql = r#"
WITH selected AS (
    SELECT * FROM task_categories WHERE id=$2
), updated AS (
    UPDATE tasks SET category_id=$2
    WHERE id=$1 AND deleted_at IS NULL AND EXISTS (SELECT 1 FROM selected)
    RETURNING id
)
SELECT selected.id, selected.name, selected.nkey,
       (SELECT count(*) FROM tasks WHERE category_id=selected.id AND deleted_at IS NULL),
       selected.created_at, selected.updated_at
FROM selected, updated"#;

        let translated = DmlCteTranslator::translate(sql).expect("translation failed");
        assert!(translated.starts_with("UPDATE tasks SET category_id=$2"));
        assert!(translated.contains("EXISTS (SELECT 1 FROM task_categories WHERE id=$2)"));
        assert!(translated.contains("RETURNING (SELECT id FROM task_categories WHERE id=$2) AS id"));
    }

    #[test]
    fn test_translate_insert_count_cte() {
        let sql = r#"
WITH requested(company_id, position) AS (
    SELECT company_id, position
    FROM unnest($2::bigint[]) WITH ORDINALITY AS requested(company_id, position)
), inserted AS (
    INSERT INTO task_scope(task_id, kind, company_id, source, reason)
    SELECT $1, 'company', companies.id, 'manual', '작업 생성 시 연결된 회사'
    FROM requested
    JOIN companies ON companies.id=requested.company_id
    ORDER BY requested.position
    RETURNING company_id
)
SELECT count(*) FROM inserted"#;

        let translated = DmlCteTranslator::translate(sql).expect("translation failed");
        assert!(translated.starts_with("WITH requested(company_id, position) AS"));
        assert!(translated.contains("INSERT INTO task_scope"));
        assert!(translated.contains("RETURNING (SELECT count(*) FROM requested\n    JOIN companies ON companies.id=requested.company_id) AS count"));
    }
}
