/// SQL comment stripping utilities
/// 
/// This module provides functionality to strip SQL comments from queries
/// to prevent issues with query parsing and execution.
/// Strip SQL comments from a query
/// 
/// Removes both single-line (--) and multi-line (/* */) comments
/// while preserving string literals and their contents.
pub fn strip_sql_comments(query: &str) -> String {
    let mut result = String::with_capacity(query.len());
    let mut chars = query.chars().peekable();
    let mut in_string = false;
    let mut string_delimiter = '\0';
    
    while let Some(ch) = chars.next() {
        match ch {
            // Handle string literals
            '\'' | '"' if !in_string => {
                in_string = true;
                string_delimiter = ch;
                result.push(ch);
            }
            ch if ch == string_delimiter && in_string => {
                // Check for escaped quotes
                if chars.peek() == Some(&ch) {
                    // Escaped quote, consume both
                    result.push(ch);
                    result.push(chars.next().unwrap());
                } else {
                    // End of string
                    in_string = false;
                    string_delimiter = '\0';
                    result.push(ch);
                }
            }
            
            // Handle comments only outside of strings
            '-' if !in_string && chars.peek() == Some(&'-') => {
                // Single-line comment, skip to end of line
                chars.next(); // consume second '-'
                for c in chars.by_ref() {
                    if c == '\n' {
                        result.push('\n'); // preserve line break
                        break;
                    }
                }
            }
            '/' if !in_string && chars.peek() == Some(&'*') => {
                // Multi-line comment, skip until */
                chars.next(); // consume '*'
                let mut prev_char = '\0';
                for c in chars.by_ref() {
                    if prev_char == '*' && c == '/' {
                        break;
                    }
                    prev_char = c;
                }
                // Add a space to prevent token concatenation
                result.push(' ');
            }
            
            // Pass through everything else
            _ => result.push(ch),
        }
    }
    
    result
}

/// Split SQL script into individual statements by semicolon,
/// respecting comments, single quotes, double quotes, and dollar-quoted ($$...$$) blocks.
pub fn split_sql_statements(query: &str) -> Vec<&str> {
    let mut statements = Vec::new();
    let mut start = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut dollar_tag: Option<String> = None;
    let mut chars = query.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            continue;
        }
        if in_block_comment {
            if c == '*' && chars.peek().map(|&(_, next_c)| next_c) == Some('/') {
                chars.next();
                in_block_comment = false;
            }
            continue;
        }

        if in_single_quote {
            if c == '\'' {
                if chars.peek().map(|&(_, next_c)| next_c) == Some('\'') {
                    chars.next(); // skip escaped quote ''
                } else {
                    in_single_quote = false;
                }
            }
            continue;
        }

        if in_double_quote {
            if c == '"' {
                if chars.peek().map(|&(_, next_c)| next_c) == Some('"') {
                    chars.next(); // skip escaped quote ""
                } else {
                    in_double_quote = false;
                }
            }
            continue;
        }

        if let Some(ref tag) = dollar_tag {
            if query[i..].starts_with(tag) {
                let advance = tag.chars().count() - 1;
                for _ in 0..advance {
                    chars.next();
                }
                dollar_tag = None;
            }
            continue;
        }

        // Outside quotes / comments / dollar blocks:
        if c == '-' && chars.peek().map(|&(_, next_c)| next_c) == Some('-') {
            chars.next();
            in_line_comment = true;
            continue;
        }
        if c == '/' && chars.peek().map(|&(_, next_c)| next_c) == Some('*') {
            chars.next();
            in_block_comment = true;
            continue;
        }
        if c == '\'' {
            in_single_quote = true;
            continue;
        }
        if c == '"' {
            in_double_quote = true;
            continue;
        }
        if c == '$' {
            let rest = &query[i + 1..];
            if let Some(end_dollar) = rest.find('$') {
                let inner = &rest[..end_dollar];
                if inner.is_empty() || inner.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
                    let tag = format!("${inner}$");
                    let advance = tag.chars().count() - 1;
                    for _ in 0..advance {
                        chars.next();
                    }
                    dollar_tag = Some(tag);
                    continue;
                }
            }
        }
        if c == ';' {
            let stmt = query[start..i].trim();
            if !stmt.is_empty() {
                statements.push(stmt);
            }
            start = i + c.len_utf8();
        }
    }

    let remaining = query[start..].trim();
    if !remaining.is_empty() {
        statements.push(remaining);
    }

    statements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_single_line_comments() {
        assert_eq!(
            strip_sql_comments("SELECT * FROM users -- get all users"),
            "SELECT * FROM users "
        );
        
        assert_eq!(
            strip_sql_comments("SELECT * FROM users\n-- This is a comment\nWHERE id = 1"),
            "SELECT * FROM users\n\nWHERE id = 1"
        );
    }

    #[test]
    fn test_strip_multi_line_comments() {
        assert_eq!(
            strip_sql_comments("SELECT /* all columns */ * FROM users"),
            "SELECT   * FROM users"
        );
        
        assert_eq!(
            strip_sql_comments("SELECT * /* multi\nline\ncomment */ FROM users"),
            "SELECT *   FROM users"
        );
    }

    #[test]
    fn test_preserve_strings() {
        assert_eq!(
            strip_sql_comments("SELECT '--not a comment' FROM users"),
            "SELECT '--not a comment' FROM users"
        );
        
        assert_eq!(
            strip_sql_comments("SELECT '/* also not a comment */' FROM users"),
            "SELECT '/* also not a comment */' FROM users"
        );
        
        assert_eq!(
            strip_sql_comments("SELECT \"--double quoted\" FROM users"),
            "SELECT \"--double quoted\" FROM users"
        );
    }

    #[test]
    fn test_escaped_quotes() {
        assert_eq!(
            strip_sql_comments("SELECT 'It''s a test' FROM users -- comment"),
            "SELECT 'It''s a test' FROM users "
        );
        
        assert_eq!(
            strip_sql_comments("SELECT \"She said \"\"Hello\"\"\" FROM users"),
            "SELECT \"She said \"\"Hello\"\"\" FROM users"
        );
    }

    #[test]
    fn test_mixed_comments() {
        let query = r#"
-- Initial comment
SELECT 
    id, -- user id
    name, /* user name */
    email
FROM users
/* WHERE clause */
WHERE active = true -- only active users
"#;
        
        let expected = r#"

SELECT 
    id, 
    name,  
    email
FROM users
 
WHERE active = true 
"#;
        
        assert_eq!(strip_sql_comments(query), expected);
    }

    #[test]
    fn test_nested_comments() {
        // PostgreSQL doesn't support nested comments, but we should handle them gracefully
        assert_eq!(
            strip_sql_comments("SELECT /* outer /* inner */ comment */ * FROM users"),
            "SELECT   comment */ * FROM users"
        );
    }

    #[test]
    fn test_comment_like_operators() {
        // Make sure we don't strip things that look like comments but aren't
        assert_eq!(
            strip_sql_comments("SELECT * FROM users WHERE data->>'type' = 'admin'"),
            "SELECT * FROM users WHERE data->>'type' = 'admin'"
        );
        
        assert_eq!(
            strip_sql_comments("SELECT * FROM users WHERE count --> 5"),
            "SELECT * FROM users WHERE count "
        );
    }
}