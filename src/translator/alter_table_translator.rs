use sqlparser::ast::{AlterTable, AlterTableOperation, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// An individual action parsed from an ALTER TABLE statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTableAction {
    /// Drop a column, optionally ignoring if it does not exist.
    DropColumn {
        table_name: String,
        column_name: String,
        if_exists: bool,
    },
    /// Add a column, optionally ignoring if it already exists.
    AddColumn {
        table_name: String,
        column_name: String,
        pg_type: String,
        sqlite_type: String,
        sql: String,
        if_not_exists: bool,
    },
    /// Rename a column.
    RenameColumn {
        table_name: String,
        old_column_name: String,
        new_column_name: String,
        sql: String,
    },
    /// Rename a table.
    RenameTable {
        old_table_name: String,
        new_table_name: String,
        sql: String,
    },
    /// Raw SQL fallback.
    RawSql {
        sql: String,
    },
}

pub struct AlterTableTranslator;

impl AlterTableTranslator {
    /// Parse an ALTER TABLE query into one or more sequential SQLite actions.
    pub fn parse(query: &str) -> Option<Vec<AlterTableAction>> {
        let trimmed = query.trim().trim_end_matches(';').trim();
        if !trimmed.to_uppercase().starts_with("ALTER TABLE") {
            return None;
        }

        let statements = Parser::parse_sql(&PostgreSqlDialect {}, trimmed).ok()?;
        if statements.len() != 1 {
            return None;
        }

        if let Statement::AlterTable(AlterTable { name, operations, .. }) = &statements[0] {
            let table_name = name.to_string();
            let mut actions = Vec::new();

            for op in operations {
                match op {
                    AlterTableOperation::DropColumn { column_names, if_exists, .. } => {
                        for col in column_names {
                            actions.push(AlterTableAction::DropColumn {
                                table_name: table_name.clone(),
                                column_name: col.value.clone(),
                                if_exists: *if_exists,
                            });
                        }
                    }
                    AlterTableOperation::AddColumn { column_def, if_not_exists, .. } => {
                        let col_name = column_def.name.value.clone();
                        let pg_type = column_def.data_type.to_string();
                        let sqlite_type = crate::translator::CastTranslator::postgres_to_sqlite_type(&pg_type).to_string();

                        let mut sql = format!("ALTER TABLE \"{}\" ADD COLUMN \"{}\" {}", table_name.trim_matches('"'), col_name.trim_matches('"'), sqlite_type);
                        for opt in &column_def.options {
                            use sqlparser::ast::ColumnOption;
                            match &opt.option {
                                ColumnOption::Default(expr) => {
                                    sql.push_str(&format!(" DEFAULT {}", expr));
                                }
                                ColumnOption::NotNull => {
                                    sql.push_str(" NOT NULL");
                                }
                                _ => {}
                            }
                        }

                        actions.push(AlterTableAction::AddColumn {
                            table_name: table_name.clone(),
                            column_name: col_name,
                            pg_type,
                            sqlite_type,
                            sql,
                            if_not_exists: *if_not_exists,
                        });
                    }
                    AlterTableOperation::RenameColumn { old_column_name, new_column_name } => {
                        let sql = format!(
                            "ALTER TABLE \"{}\" RENAME COLUMN \"{}\" TO \"{}\"",
                            table_name.trim_matches('"'), old_column_name.value.trim_matches('"'), new_column_name.value.trim_matches('"')
                        );
                        actions.push(AlterTableAction::RenameColumn {
                            table_name: table_name.clone(),
                            old_column_name: old_column_name.value.clone(),
                            new_column_name: new_column_name.value.clone(),
                            sql,
                        });
                    }
                    AlterTableOperation::RenameTable { table_name: new_name } => {
                        let sql = format!("ALTER TABLE \"{}\" RENAME TO \"{}\"", table_name.trim_matches('"'), new_name.to_string().trim_matches('"'));
                        actions.push(AlterTableAction::RenameTable {
                            old_table_name: table_name.clone(),
                            new_table_name: new_name.to_string(),
                            sql,
                        });
                    }
                    _ => {}
                }
            }

            if !actions.is_empty() {
                return Some(actions);
            }
        }

        None
    }

    /// Check if a column exists in a SQLite table.
    pub fn column_exists(conn: &rusqlite::Connection, table_name: &str, column_name: &str) -> Result<bool, rusqlite::Error> {
        let clean_table = table_name.trim_matches('"');
        let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{clean_table}\")"))?;
        let col_lower = column_name.trim_matches('"').to_lowercase();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name.to_lowercase() == col_lower {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Execute alter table actions against a rusqlite Connection.
    pub fn execute_actions(conn: &rusqlite::Connection, actions: &[AlterTableAction]) -> Result<(), rusqlite::Error> {
        for action in actions {
            match action {
                AlterTableAction::DropColumn { table_name, column_name, if_exists } => {
                    let clean_tbl = table_name.trim_matches('"');
                    let clean_col = column_name.trim_matches('"');
                    let exists = Self::column_exists(conn, clean_tbl, clean_col)?;
                    if exists {
                        conn.execute(&format!("ALTER TABLE \"{clean_tbl}\" DROP COLUMN \"{clean_col}\""), [])?;
                        let _ = conn.execute(
                            "DELETE FROM __pgsqlite_schema WHERE lower(table_name) = lower(?1) AND lower(column_name) = lower(?2)",
                            rusqlite::params![clean_tbl, clean_col],
                        );
                    } else if !if_exists {
                        return Err(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                            Some(format!("no such column: \"{clean_col}\"")),
                        ));
                    }
                }
                AlterTableAction::AddColumn { table_name, column_name, pg_type, sqlite_type, sql, if_not_exists } => {
                    let clean_tbl = table_name.trim_matches('"');
                    let clean_col = column_name.trim_matches('"');
                    let exists = Self::column_exists(conn, clean_tbl, clean_col)?;
                    if exists {
                        if !if_not_exists {
                            return Err(rusqlite::Error::SqliteFailure(
                                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                                Some(format!("duplicate column name: {clean_col}")),
                            ));
                        }
                    } else {
                        conn.execute(sql, [])?;
                        let _ = conn.execute(
                            "INSERT OR REPLACE INTO __pgsqlite_schema (table_name, column_name, pg_type, sqlite_type) VALUES (lower(?1), lower(?2), ?3, ?4)",
                            rusqlite::params![clean_tbl, clean_col, pg_type, sqlite_type],
                        );
                    }
                }
                AlterTableAction::RenameColumn { table_name, old_column_name, new_column_name, sql } => {
                    let clean_tbl = table_name.trim_matches('"');
                    let clean_old = old_column_name.trim_matches('"');
                    let clean_new = new_column_name.trim_matches('"');
                    conn.execute(sql, [])?;
                    let _ = conn.execute(
                        "UPDATE __pgsqlite_schema SET column_name = lower(?1) WHERE lower(table_name) = lower(?2) AND lower(column_name) = lower(?3)",
                        rusqlite::params![clean_new, clean_tbl, clean_old],
                    );
                }
                AlterTableAction::RenameTable { old_table_name, new_table_name, sql } => {
                    let clean_old = old_table_name.trim_matches('"');
                    let clean_new = new_table_name.trim_matches('"');
                    conn.execute(sql, [])?;
                    let _ = conn.execute(
                        "UPDATE __pgsqlite_schema SET table_name = lower(?1) WHERE lower(table_name) = lower(?2)",
                        rusqlite::params![clean_new, clean_old],
                    );
                }
                AlterTableAction::RawSql { sql } => {
                    conn.execute(sql, [])?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_multi_drop_column() {
        let sql = "ALTER TABLE llm_records DROP COLUMN IF EXISTS raw_request, DROP COLUMN IF EXISTS raw_response";
        let actions = AlterTableTranslator::parse(sql).expect("parse failed");
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0],
            AlterTableAction::DropColumn {
                table_name: "llm_records".to_string(),
                column_name: "raw_request".to_string(),
                if_exists: true,
            }
        );
        assert_eq!(
            actions[1],
            AlterTableAction::DropColumn {
                table_name: "llm_records".to_string(),
                column_name: "raw_response".to_string(),
                if_exists: true,
            }
        );
    }

    #[test]
    fn test_parse_add_column_if_not_exists() {
        let sql = "ALTER TABLE llm_records ADD COLUMN IF NOT EXISTS task_id TEXT";
        let actions = AlterTableTranslator::parse(sql).expect("parse failed");
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            AlterTableAction::AddColumn { table_name, column_name, if_not_exists, .. } => {
                assert_eq!(table_name, "llm_records");
                assert_eq!(column_name, "task_id");
                assert!(if_not_exists);
            }
            _ => panic!("unexpected action"),
        }
    }
}
