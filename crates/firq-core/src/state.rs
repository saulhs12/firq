use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::api::{Task, TenantCount, TenantKey};

pub(crate) const QUEUE_TIME_BUCKETS_NS: [u64; 12] = [
    10_000,
    50_000,
    100_000,
    500_000,
    1_000_000,
    5_000_000,
    10_000_000,
    50_000_000,
    100_000_000,
    500_000_000,
    1_000_000_000,
    u64::MAX,
];

#[derive(Debug)]
pub(crate) struct ActiveRing<T: Copy> {
    buf: Vec<T>,
    head: usize,
}

impl<T: Copy> ActiveRing<T> {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            head: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len().saturating_sub(self.head)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn push_back(&mut self, item: T) {
        if self.head == self.buf.len() {
            self.buf.clear();
            self.head = 0;
        }
        self.buf.push(item);
    }

    pub(crate) fn pop_front(&mut self) -> Option<T> {
        if self.head >= self.buf.len() {
            return None;
        }
        let item = self.buf[self.head];
        self.head += 1;
        if self.head > 64 && self.head * 2 >= self.buf.len() {
            self.buf.drain(0..self.head);
            self.head = 0;
        } else if self.head == self.buf.len() {
            self.buf.clear();
            self.head = 0;
        }
        Some(item)
    }
}

#[derive(Debug)]
pub(crate) struct TenantState<T> {
    pub(crate) queues: [VecDeque<Task<T>>; 3],
    pub(crate) deficits: [i64; 3],
    pub(crate) quantum: i64,
    pub(crate) active: [bool; 3],
}

impl<T> TenantState<T> {
    pub(crate) fn new(quantum: i64) -> Self {
        Self {
            queues: std::array::from_fn(|_| VecDeque::new()),
            deficits: [0; 3],
            quantum,
            active: [false; 3],
        }
    }

    pub(crate) fn total_len(&self) -> usize {
        self.queues.iter().map(|queue| queue.len()).sum()
    }
}

#[derive(Debug)]
pub(crate) struct Shard<T> {
    pub(crate) tenants: HashMap<TenantKey, TenantState<T>>,
    pub(crate) active_rings: [ActiveRing<TenantKey>; 3],
}

impl<T> Shard<T> {
    pub(crate) fn new() -> Self {
        Self {
            tenants: HashMap::new(),
            active_rings: std::array::from_fn(|_| ActiveRing::new()),
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
    pub(crate) queue_time_buckets: [AtomicU64; QUEUE_TIME_BUCKETS_NS.len()],
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
            queue_time_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TopTenants {
    capacity: usize,
    entries: Vec<TenantCount>,
}

impl TopTenants {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Vec::new(),
        }
    }

    pub(crate) fn record(&mut self, tenant: TenantKey, delta: u64) {
        if self.capacity == 0 {
            return;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.tenant == tenant) {
            entry.count = entry.count.saturating_add(delta);
            return;
        }
        if self.entries.len() < self.capacity {
            self.entries.push(TenantCount {
                tenant,
                count: delta,
            });
            return;
        }
        let (min_idx, min_count) = self.entries.iter().enumerate().fold(
            (0, u64::MAX),
            |(min_idx, min_val), (idx, entry)| {
                if entry.count < min_val {
                    (idx, entry.count)
                } else {
                    (min_idx, min_val)
                }
            },
        );
        self.entries[min_idx] = TenantCount {
            tenant,
            count: min_count.saturating_add(delta),
        };
    }

    pub(crate) fn snapshot(&self) -> Vec<TenantCount> {
        let mut entries = self.entries.clone();
        entries.sort_by(|a, b| b.count.cmp(&a.count));
        entries
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
        let mut guard = self.mutex.lock();
        while self.seq.load(Ordering::Acquire) == last_seen {
            self.condvar.wait(&mut guard);
        }
    }

    pub(crate) fn wait_for_change_timeout(&self, last_seen: u64, timeout: Duration) -> bool {
        let mut guard = self.mutex.lock();
        if self.seq.load(Ordering::Acquire) != last_seen {
            return true;
        }
        self.condvar.wait_for(&mut guard, timeout);
        self.seq.load(Ordering::Acquire) != last_seen
    }
}
