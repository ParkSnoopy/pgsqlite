// Module for session management
pub mod state;
pub mod pool;
pub mod db_handler;
pub mod read_only_handler;
pub mod query_router;
pub mod portal_manager;
pub mod connection_manager;
pub mod thread_local_cache;
pub mod advisory_lock;

pub use state::{SessionState, PreparedStatement, Portal, GLOBAL_QUERY_CACHE};
pub use pool::{SqlitePool, PooledConnection};
pub use db_handler::{DbHandler, DbResponse};
pub use read_only_handler::{ReadOnlyDbHandler, ReadOnlyError};
pub use pool::PoolStats;
pub use query_router::{QueryRouter, QueryRoute, QueryType, RouterError, RouterStats};
pub use portal_manager::{PortalManager, PortalExecutor, ManagedPortal, PortalExecutionState, CachedQueryResult};
pub use connection_manager::{ConnectionManager, CURRENT_SESSION, CurrentSessionGuard, set_current_session, get_current_session};
pub use thread_local_cache::ThreadLocalConnectionCache;
pub use advisory_lock::{AdvisoryLockManager, ADVISORY_LOCK_MANAGER};