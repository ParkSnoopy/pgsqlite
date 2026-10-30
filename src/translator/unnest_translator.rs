use crate::PgSqliteError;
use crate::translator::{TranslationMetadata, ColumnTypeHint, ExpressionType};
use crate::types::PgType;
use regex::Regex;
use once_cell::sync::Lazy;
use tracing::debug;

/// Regex patterns for unnest function calls
static UNNEST_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bunnest\s*\(\s*([^)]+)\s*\)").unwrap()
});

static UNNEST_JOIN_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:CROSS\s+JOIN|JOIN)\s+(?:LATERAL\s+)?unnest\s*\(\s*([^)]+)\s*\)(?:\s+(?:AS\s+)?([a-zA-Z_][a-zA-Z0-9_]*)(?:\s*\(\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*\))?)?").unwrap()
});

static UNNEST_FROM_CLAUSE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bFROM\s+unnest\s*\(\s*([^)]+)\s*\)(?:\s+(?:AS\s+)?([a-zA-Z_][a-zA-Z0-9_]*)(?:\s*\(\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*\))?)?").unwrap()
});

static UNNEST_WITH_ORDINALITY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bFROM\s+unnest\s*\(\s*([^)]+)\s*\)\s+WITH\s+ORDINALITY(?:\s+(?:AS\s+)?([a-zA-Z_][a-zA-Z0-9_]*)(?:\s*\(\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*,\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*\))?)?").unwrap()
});

/// Translates PostgreSQL unnest() function calls to SQLite json_each() equivalents
pub struct UnnestTranslator;

impl UnnestTranslator {
    /// Check if SQL contains unnest function calls
    pub fn contains_unnest(sql: &str) -> bool {
        // Fast path: check for unnest before any expensive operations
        if !sql.contains("unnest") && !sql.contains("UNNEST") {
            return false;
        }
        
        // Only do lowercase conversion if unnest is present
        let sql_lower = sql.to_lowercase();
        sql_lower.contains("unnest(")
    }
    
    /// Translate unnest() function calls to json_each() equivalents
    pub fn translate_unnest(sql: &str) -> Result<String, PgSqliteError> {
        if !Self::contains_unnest(sql) {
            return Ok(sql.to_string());
        }
        
        let mut result = sql.to_string();
        
        // Handle different patterns:
        // 1. [CROSS] JOIN [LATERAL] unnest(array) AS alias(col)
        // 2. FROM unnest(array) WITH ORDINALITY AS alias(col1, col2)
        // 3. FROM unnest(array) AS alias(col)
        // 4. unnest(array) in SELECT clause
        
        result = Self::translate_join_clause(&result)?;
        result = Self::translate_from_clause_with_ordinality(&result)?;
        result = Self::translate_from_clause(&result)?;
        result = Self::translate_select_clause(&result)?;
        
        Ok(result)
    }
    
    /// Translate unnest with metadata
    pub fn translate_with_metadata(sql: &str) -> Result<(String, TranslationMetadata), PgSqliteError> {
        if !Self::contains_unnest(sql) {
            return Ok((sql.to_string(), TranslationMetadata::new()));
        }
        
        let mut result = sql.to_string();
        let mut metadata = TranslationMetadata::new();
        
        // Translate unnest calls
        result = Self::translate_join_clause(&result)?;
        result = Self::translate_from_clause_with_ordinality(&result)?;
        result = Self::translate_from_clause(&result)?;
        result = Self::translate_select_clause(&result)?;
        
        // Extract metadata for aliased unnest functions
        Self::extract_unnest_metadata(&result, &mut metadata);
        
        Ok((result, metadata))
    }
    
    /// Translate [CROSS ]JOIN [LATERAL] unnest(...) AS alias(col) to JOIN json_each(...) AS alias
    fn translate_join_clause(sql: &str) -> Result<String, PgSqliteError> {
        let mut result = sql.to_string();
        let mut replacements = Vec::new();
        let mut col_replacements = Vec::new();

        for captures in UNNEST_JOIN_REGEX.captures_iter(&result) {
            let array_expr = captures[1].trim();
            let table_alias = captures.get(2).map(|m| m.as_str()).unwrap_or("unnest_table");
            let col_alias = captures.get(3).map(|m| m.as_str());

            let json_target = if array_expr.starts_with("array_to_json(") || array_expr.starts_with('\'') || array_expr.starts_with('[') || array_expr.starts_with("ARRAY[") {
                array_expr.to_string()
            } else {
                format!("array_to_json({array_expr})")
            };

            let replacement = format!("JOIN json_each({json_target}) AS {table_alias}");
            replacements.push((captures[0].to_string(), replacement));

            if let Some(col) = col_alias {
                col_replacements.push((table_alias.to_string(), col.to_string()));
            }
        }

        for (original, replacement) in replacements {
            result = result.replace(&original, &replacement);
            debug!("Translated JOIN unnest: {} -> {}", original, replacement);
        }

        for (table_alias, col_alias) in col_replacements {
            let pat = format!(r"(?i)\b{}\.{}\b", regex::escape(&table_alias), regex::escape(&col_alias));
            if let Ok(re) = Regex::new(&pat) {
                result = re.replace_all(&result, format!("{}.value", table_alias)).to_string();
            }
        }

        Ok(result)
    }

    /// Translate FROM unnest(array) AS alias to FROM json_each(array) AS alias
    fn translate_from_clause(sql: &str) -> Result<String, PgSqliteError> {
        let mut result = sql.to_string();
        
        // Collect replacements to avoid borrowing issues
        let mut replacements = Vec::new();
        for captures in UNNEST_FROM_CLAUSE_REGEX.captures_iter(&result) {
            let array_expr = captures[1].trim();
            let alias = captures.get(2).map(|m| m.as_str()).unwrap_or("unnest_table");
            let col_alias = captures.get(3).map(|m| m.as_str());
            
            let json_target = if array_expr.starts_with("array_to_json(") || array_expr.starts_with('\'') || array_expr.starts_with('[') || array_expr.starts_with("ARRAY[") {
                array_expr.to_string()
            } else {
                format!("array_to_json({array_expr})")
            };

            let replacement = if let Some(col) = col_alias {
                format!("FROM (SELECT value AS {col} FROM json_each({json_target})) AS {alias}")
            } else {
                format!("FROM json_each({json_target}) AS {alias}")
            };
            
            replacements.push((captures[0].to_string(), replacement));
        }
        
        // Apply replacements
        for (original, replacement) in replacements {
            result = result.replace(&original, &replacement);
            debug!("Translated FROM unnest: {} -> {}", original, replacement);
        }
        
        Ok(result)
    }
    
    /// Translate FROM unnest(array) WITH ORDINALITY AS alias to CTE with ROW_NUMBER
    fn translate_from_clause_with_ordinality(sql: &str) -> Result<String, PgSqliteError> {
        let mut result = sql.to_string();
        
        // Collect replacements to avoid borrowing issues
        let mut replacements = Vec::new();
        for captures in UNNEST_WITH_ORDINALITY_REGEX.captures_iter(&result) {
            let array_expr = captures[1].trim();
            let alias = captures.get(2).map(|m| m.as_str()).unwrap_or("unnest_table");
            let col1 = captures.get(3).map(|m| m.as_str()).unwrap_or("value");
            let col2 = captures.get(4).map(|m| m.as_str()).unwrap_or("ordinality");
            
            let json_target = if array_expr.starts_with("array_to_json(") || array_expr.starts_with('\'') || array_expr.starts_with('[') || array_expr.starts_with("ARRAY[") {
                array_expr.to_string()
            } else {
                format!("array_to_json({array_expr})")
            };

            let replacement = format!(
                "FROM (SELECT value AS {col1}, (key + 1) AS {col2} FROM json_each({json_target})) AS {alias}"
            );
            
            replacements.push((captures[0].to_string(), replacement));
        }
        
        // Apply replacements
        for (original, replacement) in replacements {
            result = result.replace(&original, &replacement);
            debug!("Translated FROM unnest WITH ORDINALITY: {} -> {}", original, replacement);
        }
        
        Ok(result)
    }
    
    /// Translate unnest() calls in SELECT clause to subqueries with json_each
    fn translate_select_clause(sql: &str) -> Result<String, PgSqliteError> {
        let mut result = sql.to_string();
        
        // Handle unnest() in SELECT clause - this is more complex
        // For now, we'll provide a basic translation
        let mut replacements = Vec::new();
        for captures in UNNEST_REGEX.captures_iter(&result) {
            let array_expr = captures[1].trim();
            
            // This is a simplified translation that works for basic cases
            // More complex cases might need different handling
            let replacement = format!("(SELECT value FROM json_each({array_expr}))");
            
            replacements.push((captures[0].to_string(), replacement));
        }
        
        // Apply replacements
        for (original, replacement) in replacements {
            result = result.replace(&original, &replacement);
            debug!("Translated SELECT unnest: {} -> {}", original, replacement);
        }
        
        Ok(result)
    }
    
    /// Extract metadata for aliased unnest functions
    fn extract_unnest_metadata(sql: &str, metadata: &mut TranslationMetadata) {
        // Look for aliased unnest functions (now converted to json_each)
        let alias_regex = Regex::new(r"(?i)json_each\s*\([^)]+\)\s+(?:AS\s+)?(\w+)").unwrap();
        
        for captures in alias_regex.captures_iter(sql) {
            let alias = captures[1].to_string();
            debug!("Found json_each (unnest) alias: {}", alias);
            
            metadata.add_hint(alias, ColumnTypeHint {
                source_column: None,
                suggested_type: Some(PgType::Text), // json_each returns text values
                datetime_subtype: None,
                is_expression: true,
                expression_type: Some(ExpressionType::Other),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_unnest_from_clause() {
        let sql = "SELECT value FROM unnest(ARRAY[1,2,3]) AS t";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("json_each"));
        assert!(!result.contains("unnest"));
    }
    
    #[test]
    fn test_unnest_from_clause_with_alias() {
        let sql = "SELECT t.value FROM unnest('[1,2,3]'::json) AS t";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("json_each('[1,2,3]'::json) AS t"));
    }
    
    #[test]
    fn test_unnest_select_clause() {
        let sql = "SELECT unnest(tags) FROM articles";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("(SELECT value FROM json_each(tags))"));
    }
    
    #[test]
    fn test_no_unnest() {
        let sql = "SELECT name FROM users";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert_eq!(result, "SELECT name FROM users");
    }
    
    #[test]
    fn test_contains_unnest() {
        assert!(UnnestTranslator::contains_unnest("SELECT unnest(array) FROM table"));
        assert!(UnnestTranslator::contains_unnest("FROM unnest(array) AS t"));
        assert!(!UnnestTranslator::contains_unnest("SELECT name FROM users"));
        
        // Test the specific query from the integration test
        assert!(UnnestTranslator::contains_unnest("SELECT value FROM unnest('[\"first\", \"second\", \"third\"]') AS t"));
    }
    
    #[test]
    fn test_unnest_with_metadata() {
        let sql = "SELECT value FROM unnest('[1,2,3]') AS expanded";
        let (result, _metadata) = UnnestTranslator::translate_with_metadata(sql).unwrap();
        assert!(result.contains("json_each"));
        // The metadata should contain hints for the alias if it's a table alias
    }
    
    #[test]
    fn test_unnest_with_ordinality() {
        let sql = "SELECT value, ordinality FROM unnest('[1,2,3]') WITH ORDINALITY AS t";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("json_each"));
        assert!(result.contains("ordinality"));
        assert!(result.contains("(key + 1)"));
        assert!(!result.contains("WITH ORDINALITY"));
    }
    
    #[test]
    fn test_unnest_with_ordinality_no_alias() {
        let sql = "SELECT value, ordinality FROM unnest('[1,2,3]') WITH ORDINALITY";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("json_each"));
        assert!(result.contains("AS unnest_table"));
        assert!(result.contains("ordinality"));
    }
    
    #[test]
    fn test_unnest_with_ordinality_complex() {
        let sql = "SELECT t.value, t.ordinality FROM unnest(ARRAY['a','b','c']) WITH ORDINALITY AS t WHERE t.ordinality > 1";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("json_each"));
        assert!(result.contains("(key + 1) AS ordinality"));
        assert!(!result.contains("WITH ORDINALITY"));
    }
    
    #[test]
    fn test_unnest_with_ordinality_uppercase() {
        let sql = "SELECT value, ordinality FROM UNNEST('[\"first\", \"second\", \"third\"]') WITH ORDINALITY AS t ORDER BY ordinality";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("json_each"));
        assert!(result.contains("(key + 1) AS ordinality"));
        assert!(!result.contains("WITH ORDINALITY"));
    }
    
    #[test]
    fn test_integration_test_query() {
        let sql = "SELECT value FROM unnest('[\"first\", \"second\", \"third\"]') AS t";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("json_each"));
        assert!(!result.contains("unnest"));
    }

    #[test]
    fn test_unnest_cross_join_lateral() {
        let sql = "SELECT task.id FROM assets asset CROSS JOIN LATERAL unnest(asset.task_ids) AS requested(task_id) JOIN tasks task ON task.id=requested.task_id";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("JOIN json_each(array_to_json(asset.task_ids)) AS requested"));
        assert!(result.contains("task.id=requested.value"));
        assert!(!result.contains("LATERAL"));
        assert!(!result.contains("unnest"));
    }

    #[test]
    fn test_unnest_with_ordinality_column_aliases() {
        let sql = "SELECT company_id, position FROM unnest($2::bigint[]) WITH ORDINALITY AS requested(company_id, position)";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("SELECT value AS company_id, (key + 1) AS position FROM json_each(array_to_json($2::bigint[]))"));
        assert!(result.contains("AS requested"));
    }

    #[test]
    fn test_unnest_from_column_alias() {
        let sql = "SELECT v FROM unnest(technologies) AS t(v) WHERE v = 'test'";
        let result = UnnestTranslator::translate_unnest(sql).unwrap();
        assert!(result.contains("SELECT value AS v FROM json_each(array_to_json(technologies))"));
        assert!(result.contains("AS t"));
    }
}