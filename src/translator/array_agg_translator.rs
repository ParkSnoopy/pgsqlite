use crate::PgSqliteError;
use crate::translator::{TranslationMetadata, ColumnTypeHint, ExpressionType};
use crate::types::PgType;
use regex::Regex;
use once_cell::sync::Lazy;
use tracing::debug;

/// Regex patterns for array_agg function variants
static ARRAY_AGG_DISTINCT_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)array_agg\s*\(\s*DISTINCT\s+([^)]+)\s*\)").unwrap()
});

static ARRAY_AGG_ORDER_BY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)array_agg\s*\(\s*([^)]+?)\s+ORDER\s+BY\s+([^)]+)\s*\)").unwrap()
});

static ARRAY_AGG_DISTINCT_ORDER_BY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)array_agg\s*\(\s*DISTINCT\s+([^)]+?)\s+ORDER\s+BY\s+([^)]+)\s*\)").unwrap()
});

/// Translates PostgreSQL array_agg functions with ORDER BY and DISTINCT
pub struct ArrayAggTranslator;

impl ArrayAggTranslator {
    /// Check if SQL contains array_agg with ORDER BY or DISTINCT
    pub fn contains_enhanced_array_agg(sql: &str) -> bool {
        let sql_lower = sql.to_lowercase();
        sql_lower.contains("array_agg") && (sql_lower.contains("order by") || sql_lower.contains("distinct"))
    }

    /// Check if SQL contains string_agg with ORDER BY or DISTINCT
    pub fn contains_enhanced_string_agg(sql: &str) -> bool {
        let sql_lower = sql.to_lowercase();
        sql_lower.contains("string_agg") && (sql_lower.contains("order by") || sql_lower.contains("distinct"))
    }
    
    /// Translate array_agg and string_agg functions with ORDER BY and DISTINCT support
    pub fn translate_array_agg(sql: &str) -> Result<String, PgSqliteError> {
        let (result, _) = Self::translate_with_metadata(sql)?;
        Ok(result)
    }
    
    /// Translate array_agg and string_agg functions and return metadata
    pub fn translate_with_metadata(sql: &str) -> Result<(String, TranslationMetadata), PgSqliteError> {
        let has_array_agg = Self::contains_enhanced_array_agg(sql);
        let has_string_agg = Self::contains_enhanced_string_agg(sql);

        if !has_array_agg && !has_string_agg {
            return Ok((sql.to_string(), TranslationMetadata::new()));
        }
        
        let mut result = sql.to_string();
        let mut metadata = TranslationMetadata::new();
        
        if has_array_agg {
            result = Self::translate_distinct_order_by(&result)?;
            result = Self::translate_order_by(&result)?;
            result = Self::translate_distinct(&result)?;
            Self::extract_array_agg_metadata(&result, &mut metadata);
        }

        if has_string_agg {
            result = Self::translate_string_agg(&result)?;
            Self::extract_array_agg_metadata(&result, &mut metadata);
        }
        
        Ok((result, metadata))
    }
    
    /// Translate array_agg(DISTINCT expr ORDER BY expr2)
    fn translate_distinct_order_by(sql: &str) -> Result<String, PgSqliteError> {
        let mut result = sql.to_string();
        
        // Collect replacements first to avoid borrowing issues
        let mut replacements = Vec::new();
        for captures in ARRAY_AGG_DISTINCT_ORDER_BY_REGEX.captures_iter(&result) {
            let expr = captures[1].trim();
            let _order_by = captures[2].trim();
            
            // For DISTINCT with ORDER BY, we need to use a subquery approach
            // Since SQLite doesn't support ORDER BY in aggregate functions directly,
            // we'll use a workaround with GROUP BY and MIN/MAX for ordering
            let _complex_replacement = format!(
                "(SELECT array_agg_distinct({})\n                 FROM (\n                     SELECT DISTINCT {} \n                     FROM (SELECT {} FROM {}) \n                     ORDER BY {}\n                 ))",
                expr, expr, expr, "?", _order_by
            );
            
            // Since we can't determine the table context here, we'll use a simpler approach
            // that relies on the outer query context
            let simple_replacement = format!("array_agg_distinct({expr})");
            
            replacements.push((captures[0].to_string(), simple_replacement));
        }
        
        // Apply replacements
        for (original, replacement) in replacements {
            result = result.replace(&original, &replacement);
            debug!("Translated DISTINCT ORDER BY array_agg: {} -> {}", original, replacement);
        }
        
        Ok(result)
    }
    
    /// Translate array_agg(expr ORDER BY expr2)
    fn translate_order_by(sql: &str) -> Result<String, PgSqliteError> {
        let mut result = sql.to_string();
        
        // Collect replacements first to avoid borrowing issues
        let mut replacements = Vec::new();
        for captures in ARRAY_AGG_ORDER_BY_REGEX.captures_iter(&result) {
            let expr = captures[1].trim();
            let _order_by = captures[2].trim();
            
            // For now, we'll just use the regular array_agg and rely on the outer query's ORDER BY
            // This is a limitation that matches what's documented in the TODO
            let replacement = format!("array_agg({expr})");
            
            replacements.push((captures[0].to_string(), replacement));
        }
        
        // Apply replacements
        for (original, replacement) in replacements {
            result = result.replace(&original, &replacement);
            debug!("Translated ORDER BY array_agg: {} -> {} (ORDER BY not fully supported)", original, replacement);
        }
        
        Ok(result)
    }
    
    /// Translate array_agg(DISTINCT expr)
    fn translate_distinct(sql: &str) -> Result<String, PgSqliteError> {
        let mut result = sql.to_string();
        
        // Collect replacements first to avoid borrowing issues
        let mut replacements = Vec::new();
        for captures in ARRAY_AGG_DISTINCT_REGEX.captures_iter(&result) {
            let expr = captures[1].trim();
            let replacement = format!("array_agg_distinct({expr})");
            replacements.push((captures[0].to_string(), replacement));
        }
        
        // Apply replacements
        for (original, replacement) in replacements {
            result = result.replace(&original, &replacement);
            debug!("Translated DISTINCT array_agg: {} -> {}", original, replacement);
        }
        
        Ok(result)
    }

    /// Translate string_agg functions with ORDER BY and DISTINCT support
    pub fn translate_string_agg(sql: &str) -> Result<String, PgSqliteError> {
        let bytes = sql.as_bytes();
        let len = sql.len();
        let sql_lower = sql.to_lowercase();
        let mut result = String::with_capacity(len);
        let mut last_idx = 0;
        let mut search_from = 0;

        while let Some(rel_pos) = sql_lower[search_from..].find("string_agg") {
            let fn_start = search_from + rel_pos;

            // Check word boundary before string_agg
            if fn_start > 0 && (bytes[fn_start - 1].is_ascii_alphanumeric() || bytes[fn_start - 1] == b'_') {
                search_from = fn_start + 10;
                continue;
            }

            // Skip whitespace after string_agg
            let mut i = fn_start + 10;
            while i < len && bytes[i].is_ascii_whitespace() {
                i += 1;
            }

            if i >= len || bytes[i] != b'(' {
                search_from = fn_start + 10;
                continue;
            }

            let open_paren = i;
            // Scan for matching ')'
            let mut depth = 1;
            let mut in_quote = false;
            let mut quote_char = b' ';
            let mut j = open_paren + 1;

            while j < len && depth > 0 {
                let b = bytes[j];
                if in_quote {
                    if b == quote_char {
                        if j + 1 < len && bytes[j + 1] == quote_char {
                            j += 2;
                            continue;
                        } else {
                            in_quote = false;
                        }
                    }
                } else if b == b'\'' || b == b'"' {
                    in_quote = true;
                    quote_char = b;
                } else if b == b'(' {
                    depth += 1;
                } else if b == b')' {
                    depth -= 1;
                }
                j += 1;
            }

            if depth != 0 {
                break;
            }

            let close_paren = j - 1;
            let inside = &sql[open_paren + 1..close_paren];

            if let Some(replacement) = Self::rewrite_string_agg_args(inside) {
                result.push_str(&sql[last_idx..fn_start]);
                result.push_str(&replacement);
                last_idx = close_paren + 1;
            }

            search_from = close_paren + 1;
        }

        result.push_str(&sql[last_idx..]);
        Ok(result)
    }

    fn rewrite_string_agg_args(inside: &str) -> Option<String> {
        let trimmed = inside.trim();
        let bytes = trimmed.as_bytes();

        let (is_distinct, after_distinct) = if trimmed.len() >= 8
            && trimmed[..8].eq_ignore_ascii_case("distinct")
            && (trimmed.len() == 8 || bytes[8].is_ascii_whitespace())
        {
            (true, trimmed[8..].trim())
        } else {
            (false, trimmed)
        };

        let after_bytes = after_distinct.as_bytes();
        let after_len = after_distinct.len();
        let mut depth = 0;
        let mut in_quote = false;
        let mut quote_char = b' ';
        let mut comma_pos = None;

        let mut k = 0;
        while k < after_len {
            let b = after_bytes[k];
            if in_quote {
                if b == quote_char {
                    if k + 1 < after_len && after_bytes[k + 1] == quote_char {
                        k += 2;
                        continue;
                    } else {
                        in_quote = false;
                    }
                }
            } else if b == b'\'' || b == b'"' {
                in_quote = true;
                quote_char = b;
            } else if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth -= 1;
            } else if b == b',' && depth == 0 {
                comma_pos = Some(k);
                break;
            }
            k += 1;
        }

        let Some(comma_idx) = comma_pos else {
            return None;
        };

        let expr = after_distinct[..comma_idx].trim();
        let second_part = after_distinct[comma_idx + 1..].trim();

        let second_bytes = second_part.as_bytes();
        let second_len = second_part.len();
        let mut delim = second_part;
        let mut has_order_by = false;

        let mut depth = 0;
        let mut in_quote = false;
        let mut quote_char = b' ';
        let mut idx = 0;

        while idx < second_len {
            let b = second_bytes[idx];
            if in_quote {
                if b == quote_char {
                    if idx + 1 < second_len && second_bytes[idx + 1] == quote_char {
                        idx += 2;
                        continue;
                    } else {
                        in_quote = false;
                    }
                }
            } else if b == b'\'' || b == b'"' {
                in_quote = true;
                quote_char = b;
            } else if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth -= 1;
            } else if depth == 0
                && idx + 8 <= second_len
                && second_bytes[idx..idx + 8].eq_ignore_ascii_case(b"order by")
            {
                let before_ok = idx == 0 || second_bytes[idx - 1].is_ascii_whitespace();
                let after_ok = idx + 8 == second_len || second_bytes[idx + 8].is_ascii_whitespace();
                if before_ok && after_ok && second_part.is_char_boundary(idx) {
                    delim = second_part[..idx].trim();
                    has_order_by = true;
                    break;
                }
            }
            idx += 1;
        }

        if is_distinct {
            Some(format!("string_agg_distinct({expr}, {delim})"))
        } else if has_order_by {
            Some(format!("string_agg({expr}, {delim})"))
        } else {
            None
        }
    }
    
    /// Extract metadata for aliased array_agg and string_agg functions
    fn extract_array_agg_metadata(sql: &str, metadata: &mut TranslationMetadata) {
        // Look for aliased array_agg / string_agg functions
        let alias_regex = Regex::new(r"(?i)(?:array_agg(?:_distinct)?|string_agg(?:_distinct)?)\s*\([^)]+\)\s+(?:AS\s+)?(\w+)").unwrap();
        
        for captures in alias_regex.captures_iter(sql) {
            let alias = captures[1].to_string();
            debug!("Found aggregate alias: {}", alias);
            
            metadata.add_hint(alias, ColumnTypeHint {
                source_column: None,
                suggested_type: Some(PgType::Text),
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
    fn test_array_agg_distinct() {
        let sql = "SELECT array_agg(DISTINCT name) FROM users";
        let result = ArrayAggTranslator::translate_array_agg(sql).unwrap();
        assert_eq!(result, "SELECT array_agg_distinct(name) FROM users");
    }
    
    #[test]
    fn test_array_agg_order_by() {
        let sql = "SELECT array_agg(name ORDER BY name) FROM users";
        let result = ArrayAggTranslator::translate_array_agg(sql).unwrap();
        assert_eq!(result, "SELECT array_agg(name) FROM users");
    }
    
    #[test]
    fn test_array_agg_distinct_order_by() {
        let sql = "SELECT array_agg(DISTINCT name ORDER BY name) FROM users";
        let result = ArrayAggTranslator::translate_array_agg(sql).unwrap();
        assert_eq!(result, "SELECT array_agg_distinct(name) FROM users");
    }
    
    #[test]
    fn test_array_agg_with_alias() {
        let sql = "SELECT array_agg(DISTINCT name) AS unique_names FROM users";
        let (result, metadata) = ArrayAggTranslator::translate_with_metadata(sql).unwrap();
        assert_eq!(result, "SELECT array_agg_distinct(name) AS unique_names FROM users");
        assert!(metadata.get_hint("unique_names").is_some());
    }
    
    #[test]
    fn test_no_enhanced_array_agg() {
        let sql = "SELECT array_agg(name) FROM users";
        let result = ArrayAggTranslator::translate_array_agg(sql).unwrap();
        assert_eq!(result, "SELECT array_agg(name) FROM users");
    }
    
    #[test]
    fn test_contains_enhanced_array_agg() {
        assert!(ArrayAggTranslator::contains_enhanced_array_agg("SELECT array_agg(DISTINCT name) FROM users"));
        assert!(ArrayAggTranslator::contains_enhanced_array_agg("SELECT array_agg(name ORDER BY name) FROM users"));
        assert!(!ArrayAggTranslator::contains_enhanced_array_agg("SELECT array_agg(name) FROM users"));
        assert!(!ArrayAggTranslator::contains_enhanced_array_agg("SELECT name FROM users"));
    }

    #[test]
    fn test_string_agg_distinct() {
        let sql = "SELECT STRING_AGG(DISTINCT agent_key, ',') AS agents FROM skill_usage";
        let (result, metadata) = ArrayAggTranslator::translate_with_metadata(sql).unwrap();
        assert_eq!(result, "SELECT string_agg_distinct(agent_key, ',') AS agents FROM skill_usage");
        assert!(metadata.get_hint("agents").is_some());
    }

    #[test]
    fn test_string_agg_order_by() {
        let sql = "SELECT string_agg(name, ', ' ORDER BY id) FROM users";
        let result = ArrayAggTranslator::translate_array_agg(sql).unwrap();
        assert_eq!(result, "SELECT string_agg(name, ', ') FROM users");
    }

    #[test]
    fn test_string_agg_distinct_with_comma_in_expr() {
        let sql = "SELECT string_agg(DISTINCT COALESCE(name, 'n/a'), ',') FROM users";
        let result = ArrayAggTranslator::translate_array_agg(sql).unwrap();
        assert_eq!(result, "SELECT string_agg_distinct(COALESCE(name, 'n/a'), ',') FROM users");
    }
}