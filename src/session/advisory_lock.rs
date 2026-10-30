use std::collections::HashMap;
use parking_lot::{Mutex, Condvar};
use uuid::Uuid;
use once_cell::sync::Lazy;

pub static ADVISORY_LOCK_MANAGER: Lazy<AdvisoryLockManager> = Lazy::new(AdvisoryLockManager::new);

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
}

impl AdvisoryLockManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AdvisoryLockState {
                locks: HashMap::new(),
            }),
            cond: Condvar::new(),
        }
    }

    pub fn lock(&self, session_id: Uuid, key: i64, is_xact: bool) {
        tokio::task::block_in_place(|| {
            tracing::debug!("AdvisoryLock::lock: session={} key={} xact={}", session_id, key, is_xact);
            let mut state = self.inner.lock();
            loop {
                if let Some(holder) = state.locks.get_mut(&key) {
                    if holder.session_id == session_id {
                        tracing::debug!("AdvisoryLock::lock: re-entrant session={} key={}", session_id, key);
                        if is_xact {
                            holder.xact_count += 1;
                        } else {
                            holder.session_count += 1;
                        }
                        return;
                    } else {
                        tracing::debug!("AdvisoryLock::lock: waiting session={} owner={} key={}", session_id, holder.session_id, key);
                        self.cond.wait(&mut state);
                        tracing::debug!("AdvisoryLock::lock: woke up session={} key={}", session_id, key);
                    }
                } else {
                    tracing::debug!("AdvisoryLock::lock: acquired session={} key={}", session_id, key);
                    state.locks.insert(key, LockHolder {
                        session_id,
                        session_count: if is_xact { 0 } else { 1 },
                        xact_count: if is_xact { 1 } else { 0 },
                    });
                    return;
                }
            }
        });
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
        let mut state = self.inner.lock();
        let prev_len = state.locks.len();
        state.locks.retain(|_key, holder| holder.session_id != *session_id);
        if state.locks.len() < prev_len {
            self.cond.notify_all();
        }
    }
}
