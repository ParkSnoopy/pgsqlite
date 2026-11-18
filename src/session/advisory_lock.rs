use std::collections::{HashMap, HashSet};
use parking_lot::{Mutex, Condvar};
use uuid::Uuid;
use once_cell::sync::Lazy;

pub static ADVISORY_LOCK_MANAGER: Lazy<AdvisoryLockManager> = Lazy::new(AdvisoryLockManager::new);

/// Deterministically convert session UUID to a positive integer backend PID.
pub fn uuid_to_backend_pid(id: &Uuid) -> i32 {
    let bytes = id.as_bytes();
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    (n % 2_000_000_000 + 1) as i32
}

pub struct AdvisoryLockManager {
    inner: Mutex<AdvisoryLockState>,
    cond: Condvar,
}

#[derive(Debug, Clone)]
struct LockHolder {
    session_id: Uuid,
    session_count: u32,
    xact_count: u32,
}

struct AdvisoryLockState {
    locks: HashMap<i64, LockHolder>,
    blocking_pids: HashMap<i32, i32>,
    active_write_session: Option<(Uuid, i32)>,
    sessions_with_writes: HashSet<Uuid>,
    waiting_writers: HashMap<i64, HashSet<Uuid>>,
    waiting_for: HashMap<Uuid, Uuid>,
}

pub fn hash_table_lock(name: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("__pg_table_lock__:{}", name.to_lowercase()).hash(&mut hasher);
    hasher.finish() as i64
}

impl AdvisoryLockManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AdvisoryLockState {
                locks: HashMap::new(),
                blocking_pids: HashMap::new(),
                active_write_session: None,
                sessions_with_writes: HashSet::new(),
                waiting_writers: HashMap::new(),
                waiting_for: HashMap::new(),
            }),
            cond: Condvar::new(),
        }
    }

    pub fn on_write_executed(&self, session_id: &Uuid) {
        let mut state = self.inner.lock();
        state.sessions_with_writes.insert(*session_id);
    }

    pub fn has_written(&self, session_id: &Uuid) -> bool {
        let state = self.inner.lock();
        state.sessions_with_writes.contains(session_id)
    }

    pub fn get_blocking_pid(&self, pid: i32) -> Option<i32> {
        let state = self.inner.lock();
        state.blocking_pids.get(&pid).copied()
    }

    pub fn on_begin_start(&self, session_id: &Uuid) {
        let waiter_pid = uuid_to_backend_pid(session_id);
        let mut state = self.inner.lock();
        state.sessions_with_writes.remove(session_id);
        if let Some((holder_session, holder_pid)) = state.active_write_session {
            if holder_session != *session_id {
                state.blocking_pids.insert(waiter_pid, holder_pid);
            }
        }
    }

    pub fn on_begin_success(&self, session_id: &Uuid) {
        let session_pid = uuid_to_backend_pid(session_id);
        let mut state = self.inner.lock();
        state.sessions_with_writes.remove(session_id);
        state.blocking_pids.remove(&session_pid);
        state.active_write_session = Some((*session_id, session_pid));
    }

    pub fn on_transaction_end(&self, session_id: &Uuid) {
        let session_pid = uuid_to_backend_pid(session_id);
        let mut state = self.inner.lock();
        state.blocking_pids.remove(&session_pid);
        state.sessions_with_writes.remove(session_id);
        state.waiting_for.remove(session_id);
        for ws in state.waiting_writers.values_mut() {
            ws.remove(session_id);
        }
        state.waiting_writers.retain(|_, ws| !ws.is_empty());
        if let Some((active_session, _)) = state.active_write_session {
            if active_session == *session_id {
                state.active_write_session = None;
            }
        }
    }

    pub fn on_write_start(&self, session_id: &Uuid) {
        let waiter_pid = uuid_to_backend_pid(session_id);
        let mut state = self.inner.lock();
        if let Some((holder_session, holder_pid)) = state.active_write_session {
            if holder_session != *session_id {
                state.blocking_pids.insert(waiter_pid, holder_pid);
            }
        }
    }

    pub fn on_write_end(&self, session_id: &Uuid) {
        let waiter_pid = uuid_to_backend_pid(session_id);
        let mut state = self.inner.lock();
        if let Some((holder_session, _)) = state.active_write_session {
            if holder_session != *session_id {
                state.blocking_pids.remove(&waiter_pid);
            }
        }
    }

    pub fn lock(&self, session_id: Uuid, key: i64, is_xact: bool) -> bool {
        tokio::task::block_in_place(|| {
            let waiter_pid = uuid_to_backend_pid(&session_id);
            tracing::debug!("AdvisoryLock::lock: session={} key={} xact={}", session_id, key, is_xact);
            let mut state = self.inner.lock();
            let mut waited = false;
            loop {
                let is_writer = state.sessions_with_writes.contains(&session_id);
                if let Some(holder_session) = state.locks.get(&key).map(|h| h.session_id) {
                    if holder_session == session_id {
                        let holder = state.locks.get_mut(&key).unwrap();
                        if is_xact {
                            holder.xact_count += 1;
                        } else {
                            holder.session_count += 1;
                        }
                        state.blocking_pids.remove(&waiter_pid);
                        state.waiting_for.remove(&session_id);
                        if let Some(ws) = state.waiting_writers.get_mut(&key) {
                            ws.remove(&session_id);
                            if ws.is_empty() {
                                state.waiting_writers.remove(&key);
                            }
                        }
                        tracing::debug!("AdvisoryLock::lock: re-entrant session={} key={}", session_id, key);
                        return waited;
                    } else {
                        waited = true;
                        let holder_pid = uuid_to_backend_pid(&holder_session);
                        state.blocking_pids.insert(waiter_pid, holder_pid);
                        state.waiting_for.insert(session_id, holder_session);
                        if is_writer {
                            state.waiting_writers.entry(key).or_default().insert(session_id);
                        }
                        tracing::debug!("AdvisoryLock::lock: waiting session={} owner={} key={}", session_id, holder_session, key);
                        self.cond.wait(&mut state);
                        state.blocking_pids.remove(&waiter_pid);
                        state.waiting_for.remove(&session_id);
                        tracing::debug!("AdvisoryLock::lock: woke up session={} key={}", session_id, key);
                    }
                } else {
                    // Prioritize waiters that already hold SQLite's write lock so a
                    // non-writer cannot acquire the key first and invert lock order.
                    let waiting_writer = if !is_writer {
                        state.waiting_writers.get(&key).and_then(|ws| {
                            ws.iter()
                                .copied()
                                .find(|w| *w != session_id && state.sessions_with_writes.contains(w))
                        })
                    } else {
                        None
                    };
                    if let Some(writer_sid) = waiting_writer {
                        waited = true;
                        state.blocking_pids.insert(waiter_pid, uuid_to_backend_pid(&writer_sid));
                        state.waiting_for.insert(session_id, writer_sid);
                        self.cond.wait(&mut state);
                        state.blocking_pids.remove(&waiter_pid);
                        state.waiting_for.remove(&session_id);
                        continue;
                    }
                    state.blocking_pids.remove(&waiter_pid);
                    state.waiting_for.remove(&session_id);
                    if let Some(ws) = state.waiting_writers.get_mut(&key) {
                        ws.remove(&session_id);
                        if ws.is_empty() {
                            state.waiting_writers.remove(&key);
                        }
                    }
                    tracing::debug!("AdvisoryLock::lock: acquired session={} key={}", session_id, key);
                    state.locks.insert(key, LockHolder {
                        session_id,
                        session_count: if is_xact { 0 } else { 1 },
                        xact_count: if is_xact { 1 } else { 0 },
                    });
                    return waited;
                }
            }
        })
    }

    /// Returns true if any other session currently holding an uncommitted SQLite
    /// write transaction is waiting (directly or transitively) on an advisory lock
    /// owned by `session_id`.
    pub fn is_deadlocked_with_sqlite_writer(&self, session_id: &Uuid) -> bool {
        let state = self.inner.lock();
        for writer in &state.sessions_with_writes {
            if writer == session_id {
                continue;
            }
            let mut cur = *writer;
            for _ in 0..=state.waiting_for.len() {
                if let Some(&holder) = state.waiting_for.get(&cur) {
                    if holder == *session_id {
                        return true;
                    }
                    cur = holder;
                } else {
                    break;
                }
            }
        }
        false
    }

    pub fn try_lock(&self, session_id: Uuid, key: i64, is_xact: bool) -> bool {
        let mut state = self.inner.lock();
        if let Some(holder) = state.locks.get_mut(&key) {
            if holder.session_id == session_id {
                if is_xact {
                    holder.xact_count += 1;
                } else {
                    holder.session_count += 1;
                }
                true
            } else {
                false
            }
        } else {
            state.locks.insert(key, LockHolder {
                session_id,
                session_count: if is_xact { 0 } else { 1 },
                xact_count: if is_xact { 1 } else { 0 },
            });
            true
        }
    }

    pub fn unlock(&self, session_id: Uuid, key: i64) -> bool {
        let mut state = self.inner.lock();
        let mut released = false;
        if let Some(holder) = state.locks.get_mut(&key) {
            if holder.session_id == session_id && holder.session_count > 0 {
                holder.session_count -= 1;
                released = true;
                if holder.session_count == 0 && holder.xact_count == 0 {
                    state.locks.remove(&key);
                    self.cond.notify_all();
                }
            }
        }
        released
    }

    pub fn release_xact_locks(&self, session_id: &Uuid) {
        tracing::debug!("AdvisoryLock::release_xact_locks: session={}", session_id);
        let mut state = self.inner.lock();
        let mut any_freed = false;
        state.locks.retain(|key, holder| {
            if holder.session_id == *session_id {
                tracing::debug!("AdvisoryLock::release_xact_locks: freeing key={} from session={}", key, session_id);
                holder.xact_count = 0;
                if holder.session_count == 0 {
                    any_freed = true;
                    return false;
                }
            }
            true
        });
        if any_freed {
            tracing::debug!("AdvisoryLock::release_xact_locks: notifying waiters");
            self.cond.notify_all();
        }
    }

    pub fn release_all_session_locks(&self, session_id: &Uuid) {
        let mut state = self.inner.lock();
        let mut any_freed = false;
        state.locks.retain(|_key, holder| {
            if holder.session_id == *session_id {
                holder.session_count = 0;
                if holder.xact_count == 0 {
                    any_freed = true;
                    return false;
                }
            }
            true
        });
        if any_freed {
            self.cond.notify_all();
        }
    }

    pub fn release_all(&self, session_id: &Uuid) {
        let session_pid = uuid_to_backend_pid(session_id);
        let mut state = self.inner.lock();
        state.blocking_pids.remove(&session_pid);
        state.sessions_with_writes.remove(session_id);
        state.waiting_for.retain(|k, v| k != session_id && v != session_id);
        for ws in state.waiting_writers.values_mut() {
            ws.remove(session_id);
        }
        state.waiting_writers.retain(|_, ws| !ws.is_empty());
        if let Some((active_session, _)) = state.active_write_session {
            if active_session == *session_id {
                state.active_write_session = None;
            }
        }
        let prev_len = state.locks.len();
        state.locks.retain(|_key, holder| holder.session_id != *session_id);
        if state.locks.len() < prev_len {
            self.cond.notify_all();
        }
    }
}
