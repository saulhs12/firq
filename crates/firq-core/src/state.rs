use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crate::api::{Task, TenantKey};

#[derive(Debug)]
pub(crate) struct TenantState<T> {
    pub(crate) queue: VecDeque<Task<T>>,
    pub(crate) deficit: i64,
    pub(crate) quantum: i64,
    pub(crate) active: bool,
}

impl<T> TenantState<T> {
    pub(crate) fn new(quantum: i64) -> Self {
        Self {
            queue: VecDeque::new(),
            deficit: 0,
            quantum,
            active: false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Shard<T> {
    pub(crate) tenants: HashMap<TenantKey, TenantState<T>>,
    pub(crate) active_ring: VecDeque<TenantKey>,
}

impl<T> Shard<T> {
    pub(crate) fn new() -> Self {
        Self {
            tenants: HashMap::new(),
            active_ring: VecDeque::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct StatsCounters {
    pub(crate) enqueued: AtomicU64,
    pub(crate) dequeued: AtomicU64,
    pub(crate) expired: AtomicU64,
    pub(crate) dropped: AtomicU64,
    pub(crate) queue_len_estimate: AtomicU64,
    pub(crate) queue_time_sum_ns: AtomicU64,
    pub(crate) queue_time_samples: AtomicU64,
}

impl StatsCounters {
    pub(crate) fn new() -> Self {
        Self {
            enqueued: AtomicU64::new(0),
            dequeued: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            queue_len_estimate: AtomicU64::new(0),
            queue_time_sum_ns: AtomicU64::new(0),
            queue_time_samples: AtomicU64::new(0),
        }
    }
}

#[derive(Debug)]
pub(crate) struct WorkSignal {
    mutex: Mutex<()>,
    condvar: Condvar,
    seq: AtomicU64,
}

impl WorkSignal {
    pub(crate) fn new() -> Self {
        Self {
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
            seq: AtomicU64::new(0),
        }
    }

    pub(crate) fn current(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    pub(crate) fn notify_all(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        self.condvar.notify_all();
    }

    pub(crate) fn wait_for_change(&self, last_seen: u64) {
        let mut guard = self.mutex.lock().expect("work signal mutex poisoned");
        while self.seq.load(Ordering::Acquire) == last_seen {
            guard = self
                .condvar
                .wait(guard)
                .expect("work signal condvar poisoned");
        }
    }
}
