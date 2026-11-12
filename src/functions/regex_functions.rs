use rusqlite::{Connection, Result, functions::FunctionFlags};
use regex::Regex;
use tracing::{debug, trace};

fn get_as_string(ctx: &rusqlite::functions::Context, idx: usize) -> Result<Option<String>> {
    use rusqlite::types::ValueRef;
    match ctx.get_raw(idx) {
        ValueRef::Null => Ok(None),
        ValueRef::Text(s) => Ok(Some(String::from_utf8_lossy(s).into_owned())),
        ValueRef::Integer(i) => Ok(Some(i.to_string())),
        ValueRef::Real(r) => Ok(Some(r.to_string())),
        ValueRef::Blob(b) => Ok(Some(String::from_utf8_lossy(b).into_owned())),
    }
}

/// Register PostgreSQL-compatible regular expression functions
pub fn register_regex_functions(conn: &Connection) -> Result<()> {
    debug!("Registering regex functions");
    
    // Register case-sensitive REGEXP function
    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let Some(pattern) = get_as_string(ctx, 0)? else {
                return Ok(None);
            };
            let Some(text) = get_as_string(ctx, 1)? else {
                return Ok(None);
            };
            
            trace!("regexp('{}', '{}')", pattern, text);
            
            match Regex::new(&pattern) {
                Ok(re) => Ok(Some(re.is_match(&text))),
                Err(e) => {
                    debug!("Invalid regex pattern '{}': {}", pattern, e);
                    // PostgreSQL returns NULL for invalid patterns
                    Ok(Some(false))
                }
            }
        },
    )?;
    
    // Register case-insensitive REGEXPI function
    conn.create_scalar_function(
        "regexpi",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let Some(pattern) = get_as_string(ctx, 0)? else {
                return Ok(None);
            };
            let Some(text) = get_as_string(ctx, 1)? else {
                return Ok(None);
            };
            
            trace!("regexpi('{}', '{}')", pattern, text);
            
            // Add (?i) flag for case-insensitive matching
            let case_insensitive_pattern = format!("(?i){pattern}");
            
            match Regex::new(&case_insensitive_pattern) {
                Ok(re) => Ok(Some(re.is_match(&text))),
                Err(e) => {
                    debug!("Invalid regex pattern '{}': {}", case_insensitive_pattern, e);
                    // PostgreSQL returns NULL for invalid patterns
                    Ok(Some(false))
                }
            }
        },
    )?;
    
    debug!("Regex functions registered successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    
    #[test]
    fn test_regexp_function() {
        let conn = Connection::open_in_memory().unwrap();
        register_regex_functions(&conn).unwrap();
        
        // Test basic match
        let result: bool = conn
            .query_row("SELECT regexp('^test', 'testing')", [], |row| row.get(0))
            .unwrap();
        assert!(result);
        
        // Test no match
        let result: bool = conn
            .query_row("SELECT regexp('^test', 'nottest')", [], |row| row.get(0))
            .unwrap();
        assert!(!result);
        
        // Test email pattern
        let result: bool = conn
            .query_row("SELECT regexp('@gmail\\.com$', 'user@gmail.com')", [], |row| row.get(0))
            .unwrap();
        assert!(result);
    }
    
    #[test]
    fn test_regexpi_function() {
        let conn = Connection::open_in_memory().unwrap();
        register_regex_functions(&conn).unwrap();
        
        // Test case-insensitive match
        let result: bool = conn
            .query_row("SELECT regexpi('TEST', 'testing')", [], |row| row.get(0))
            .unwrap();
        assert!(result);
        
        // Test with mixed case
        let result: bool = conn
            .query_row("SELECT regexpi('TeSt', 'TEST')", [], |row| row.get(0))
            .unwrap();
        assert!(result);
    }
    
    #[test]
    fn test_invalid_regex() {
        let conn = Connection::open_in_memory().unwrap();
        register_regex_functions(&conn).unwrap();
        
        // Test invalid regex pattern - should return false
        let result: bool = conn
            .query_row("SELECT regexp('[invalid', 'test')", [], |row| row.get(0))
            .unwrap();
        assert!(!result);
    }

    #[test]
    fn test_regexp_integer_and_null() {
        let conn = Connection::open_in_memory().unwrap();
        register_regex_functions(&conn).unwrap();

        // Integer text argument
        let result: bool = conn
            .query_row("SELECT regexp('^[0-9]+$', 12345)", [], |row| row.get(0))
            .unwrap();
        assert!(result);

        // Integer pattern argument
        let result: bool = conn
            .query_row("SELECT regexp(123, '12345')", [], |row| row.get(0))
            .unwrap();
        assert!(result);

        // Null argument returns NULL
        let result: Option<bool> = conn
            .query_row("SELECT regexp('^[0-9]+$', NULL)", [], |row| row.get(0))
            .unwrap();
        assert!(result.is_none());
    }
}