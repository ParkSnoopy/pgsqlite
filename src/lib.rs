pub mod protocol;
pub mod session;
pub mod translator;
pub mod types;
pub mod catalog;
pub mod functions;
pub mod query;
pub mod metadata;
pub mod rewriter;
pub mod cache;
pub mod config;
pub mod ssl;
pub mod ddl;
pub mod migration;
pub mod schema_drift;
pub mod error;
pub mod validator;
pub mod optimization;
pub mod utils;
pub mod security;
#[macro_use]
pub mod profiling;

#[cfg(test)]
pub mod alloc_tracker;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PgSqliteError {
    #[error("Protocol error: {0}")]
    Protocol(String),
    
    #[error("SQL parse error: {0}")]
    SqlParse(#[from] sqlparser::parser::ParserError),
    
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    
    #[error("Type conversion error: {0}")]
    TypeConversion(String),
    
    #[error("Feature not supported: {0}")]
    NotSupported(String),
    
    #[error("Authentication failed")]
    AuthenticationFailed,
    
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Validation error: {0}")]
    Validation(#[from] error::PgError),
}

pub type Result<T> = std::result::Result<T, PgSqliteError>;

impl PgSqliteError {
    /// Get the PostgreSQL error code for this error
    pub fn pg_error_code(&self) -> &str {
        match self {
            PgSqliteError::Protocol(_) => "08P01", // protocol_violation
            PgSqliteError::SqlParse(_) => "42601", // syntax_error
            PgSqliteError::Sqlite(_) => "58000", // system_error
            PgSqliteError::TypeConversion(_) => "22P02", // invalid_text_representation
            PgSqliteError::NotSupported(_) => "0A000", // feature_not_supported
            PgSqliteError::AuthenticationFailed => "28000", // invalid_authorization_specification
            PgSqliteError::InvalidParameter(_) => "22023", // invalid_parameter_value
            PgSqliteError::Io(_) => "58030", // io_error
            PgSqliteError::Validation(pg_err) => match pg_err {
                error::PgError::NumericValueOutOfRange { .. } => "22003", // numeric_value_out_of_range
                error::PgError::StringDataRightTruncation { .. } => "22001", // string_data_right_truncation
                error::PgError::UniqueViolation { .. } => "23505", // unique_violation
                error::PgError::ForeignKeyViolation { .. } => "23503", // foreign_key_violation
                error::PgError::SyntaxError { .. } => "42601", // syntax_error
                error::PgError::Generic { code, .. } => code,
            },
        }
    }

    /// Build a PostgreSQL `ErrorResponse` preserving constraint and type SQLSTATE codes.
    pub fn to_error_response(&self, context: &str) -> protocol::ErrorResponse {
        match self {
            PgSqliteError::Validation(pg_err) => pg_err.to_error_response(),
            PgSqliteError::Sqlite(sqlite_err) => error::sqlite_error_to_pg(sqlite_err, ""),
            other => {
                let msg = format!("{context}: {other}");
                let code = if msg.contains("UNIQUE constraint failed") {
                    "23505"
                } else if msg.contains("FOREIGN KEY constraint failed") {
                    "23503"
                } else if msg.contains("NOT NULL constraint failed") {
                    "23502"
                } else if msg.contains("CHECK constraint failed") {
                    "23514"
                } else {
                    "42000"
                };
                protocol::ErrorResponse::new("ERROR".to_string(), code.to_string(), msg)
            }
        }
    }
}

// Test helper to expose connection handler
#[doc(hidden)]
pub async fn handle_test_connection(
    stream: tokio::net::TcpStream,
    _addr: std::net::SocketAddr,
) -> anyhow::Result<()> {
    use session::DbHandler;
    let db_handler = std::sync::Arc::new(DbHandler::new(":memory:")?);
    handle_test_connection_with_pool(stream, _addr, db_handler).await
}

#[doc(hidden)]
pub async fn handle_test_connection_with_pool(
    stream: tokio::net::TcpStream,
    _addr: std::net::SocketAddr,
    db_handler: std::sync::Arc<session::DbHandler>,
) -> anyhow::Result<()> {
    use tokio_util::codec::Framed;
    use futures::{SinkExt, StreamExt};
    use std::sync::Arc;
    use protocol::{PostgresCodec, FrontendMessage, BackendMessage, AuthenticationMessage, TransactionStatus, ErrorResponse};
    use session::{SessionState, ReadOnlyDbHandler, QueryRouter};
    use query::{QueryExecutor, ExtendedQueryHandler};
    use tracing::{debug, info};
    use config::Config;
    
    let codec = PostgresCodec::new();
    let mut framed = Framed::new(stream, codec);
    
    // Wait for startup message
    let startup = match framed.next().await {
        Some(Ok(FrontendMessage::StartupMessage(msg))) => msg,
        _ => return Err(anyhow::anyhow!("Expected startup message")),
    };
    
    // Extract session parameters
    let mut database = "main".to_string();
    let mut user = "postgres".to_string();
    
    for (key, value) in &startup.parameters {
        match key.as_str() {
            "database" => database = value.clone(),
            "user" => user = value.clone(),
            _ => {}
        }
    }
    
    let session = Arc::new(SessionState::new(database, user));
    let session_id = session.id;
    
    // Set the database handler for this session for proper lifecycle management
    session.set_db_handler(db_handler.clone()).await;
    
    // Create a connection for this session
    session.initialize_connection().await
        .map_err(|e| anyhow::anyhow!("Failed to create session connection: {}", e))?;

    struct SessionCleanupGuard {
        session_id: uuid::Uuid,
        db_handler: Arc<session::DbHandler>,
    }

    impl Drop for SessionCleanupGuard {
        fn drop(&mut self) {
            self.db_handler.remove_session_connection(&self.session_id);
        }
    }

    let _cleanup_guard = SessionCleanupGuard {
        session_id,
        db_handler: db_handler.clone(),
    };
    
    // Set up connection pooling infrastructure (optional - can be enabled via config)
    let config = Arc::new(Config::load());
    
    // Create QueryRouter if pooling is enabled
    let _query_router = if config.use_pooling {
        // For tests, we'll use in-memory databases
        let read_handler = Arc::new(ReadOnlyDbHandler::new(":memory:", config.clone())
            .map_err(|e| anyhow::anyhow!("Failed to create read-only handler: {}", e))?);
        Some(Arc::new(QueryRouter::new(
            db_handler.clone(),
            read_handler,
            config.clone(),
        )))
    } else {
        None
    };
    
    if config.use_pooling {
        info!("Connection pooling enabled with read/write separation (pool size: {})", config.pool_size);
    }
    
    // Send authentication OK
    framed.send(BackendMessage::Authentication(AuthenticationMessage::Ok)).await?;
    
    // Send parameter status messages
    for (key, value) in session.parameters.read().await.iter() {
        framed.send(BackendMessage::ParameterStatus {
            name: key.clone(),
            value: value.clone(),
        }).await?;
    }
    
    // Send backend key data
    framed.send(BackendMessage::BackendKeyData {
        process_id: crate::session::uuid_to_backend_pid(&session.id),
        secret_key: 12345,
    }).await?;
    
    // Send ready for query
    framed.send(BackendMessage::ReadyForQuery {
        status: TransactionStatus::Idle,
    }).await?;
    
    // Main message loop
    let result = async {
        while let Some(msg) = framed.next().await {
            let message = msg?;
            debug!("Received message: {:?}", message);
            println!("HANDLE_CONNECTION: Received message: {:?}", message);
            match message {
                FrontendMessage::Query(sql) => {
                    info!("Received Query (simple protocol): {}", sql);
                    println!("HANDLE_CONNECTION: About to call QueryExecutor::execute_query with: '{}'", sql);
                    // Execute the query with optional query routing
                    match QueryExecutor::execute_query(&mut framed, &db_handler, &session, &sql, _query_router.as_ref()).await {
                        Ok(()) => {
                            // Query executed successfully
                        }
                        Err(e) => {
                            // If we're in a transaction, mark it as failed
                            if session.in_transaction().await {
                                session.set_transaction_status(TransactionStatus::InFailedTransaction).await;
                            }
                            
                            let err = e.to_error_response("Query execution failed");
                            framed.send(BackendMessage::ErrorResponse(Box::new(err))).await?;
                        }
                    }
                    
                    // Always send ReadyForQuery after handling the query
                    framed.send(BackendMessage::ReadyForQuery {
                        status: *session.transaction_status.read().await,
                    }).await?;
                    // Flush to ensure ReadyForQuery is sent immediately
                    framed.flush().await?;
                }
                FrontendMessage::Parse { name, query, param_types } => {
                    // TODO: Extended query protocol also needs router integration
                    match ExtendedQueryHandler::handle_parse(&mut framed, &db_handler, &session, name, query, param_types).await {
                        Ok(()) => {},
                        Err(e) => {
                            let err = ErrorResponse::new(
                                "ERROR".to_string(),
                                "42000".to_string(),
                                format!("Parse failed: {e}"),
                            );
                            framed.send(BackendMessage::ErrorResponse(Box::new(err))).await?;
                        }
                    }
                }
                FrontendMessage::Bind { portal, statement, formats, values, result_formats } => {
                    match ExtendedQueryHandler::handle_bind(&mut framed, &session, portal, statement, formats, values, result_formats).await {
                        Ok(()) => {},
                        Err(e) => {
                            if session.in_transaction().await {
                                session.set_transaction_status(TransactionStatus::InFailedTransaction).await;
                            }
                            let err = e.to_error_response("Bind failed");
                            framed.send(BackendMessage::ErrorResponse(Box::new(err))).await?;
                        }
                    }
                }
                FrontendMessage::Execute { portal, max_rows } => {
                    println!("HANDLE_CONNECTION: About to call ExtendedQueryHandler::handle_execute for portal: '{}'", portal);
                    match ExtendedQueryHandler::handle_execute(&mut framed, &db_handler, &session, portal, max_rows).await {
                        Ok(()) => {},
                        Err(e) => {
                            if session.in_transaction().await {
                                session.set_transaction_status(TransactionStatus::InFailedTransaction).await;
                            }
                            let err = e.to_error_response("Execute failed");
                            framed.send(BackendMessage::ErrorResponse(Box::new(err))).await?;
                        }
                    }
                }
                FrontendMessage::Describe { typ, name } => {
                    match ExtendedQueryHandler::handle_describe(&mut framed, &session, typ, name).await {
                        Ok(()) => {},
                        Err(e) => {
                            let err = ErrorResponse::new(
                                "ERROR".to_string(),
                                "42000".to_string(),
                                format!("Describe failed: {e}"),
                            );
                            framed.send(BackendMessage::ErrorResponse(Box::new(err))).await?;
                        }
                    }
                }
                FrontendMessage::Close { typ, name } => {
                    match ExtendedQueryHandler::handle_close(&mut framed, &session, typ, name).await {
                        Ok(()) => {},
                        Err(e) => {
                            let err = ErrorResponse::new(
                                "ERROR".to_string(),
                                "42000".to_string(),
                                format!("Close failed: {e}"),
                            );
                            framed.send(BackendMessage::ErrorResponse(Box::new(err))).await?;
                        }
                    }
                }
                FrontendMessage::Sync => {
                    framed.send(BackendMessage::ReadyForQuery {
                        status: *session.transaction_status.read().await,
                    }).await?;
                    // Flush to ensure ReadyForQuery is sent immediately
                    framed.flush().await?;
                }
                FrontendMessage::Flush => {
                    framed.flush().await?;
                }
                FrontendMessage::Terminate => break,
                other => {
                    eprintln!("Unhandled message: {other:?}");
                    let err = ErrorResponse::new(
                        "ERROR".to_string(),
                        "0A000".to_string(),
                        format!("Feature not supported: {other:?}"),
                    );
                    framed.send(BackendMessage::ErrorResponse(Box::new(err))).await?;
                    framed.send(BackendMessage::ReadyForQuery {
                        status: *session.transaction_status.read().await,
                    }).await?;
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }.await;
    
    // Clean up session connection
    db_handler.remove_session_connection(&session_id);
    
    result
}