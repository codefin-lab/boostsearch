//! Time, as a dependency.
//!
//! Nothing in the cluster reads the system clock or sleeps on it (ADR
//! 0002): every timeout and every timestamp comes through a `Clock`, which
//! in production is the system clock and in the simulation a counter a
//! seed drives, so the same interleaving happens again on demand.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Milliseconds on some monotonic axis; only differences mean anything.
pub type Millis = u64;

pub trait Clock: Send + Sync {
    /// The moment now, in milliseconds, monotonic.
    fn now(&self) -> Millis;
    /// The wall-clock time, in milliseconds since the epoch, for what is
    /// shown to people; not for ordering anything.
    fn wall(&self) -> Millis;
}

/// The system clock.
pub struct SystemClock {
    started: std::time::Instant,
}

impl SystemClock {
    /// The clock a running node uses, as something to share: a `Clock` is
    /// held by everything that needs to know the time, so it is handed out
    /// behind the trait rather than as itself.
    pub fn shared() -> Arc<dyn Clock> {
        Arc::new(SystemClock { started: std::time::Instant::now() })
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Millis {
        self.started.elapsed().as_millis() as u64
    }

    fn wall(&self) -> Millis {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A clock that moves only when told to: the simulation's.
pub struct ManualClock {
    now: AtomicU64,
    wall_offset: u64,
}

impl ManualClock {
    pub fn new(start: Millis) -> Arc<ManualClock> {
        Arc::new(ManualClock { now: AtomicU64::new(start), wall_offset: 1_700_000_000_000 })
    }

    /// Move time forward by this much.
    pub fn advance(&self, by: Millis) {
        self.now.fetch_add(by, Ordering::AcqRel);
    }

    /// Set the time outright, forward or back: a clock that jumps.
    pub fn set(&self, to: Millis) {
        self.now.store(to, Ordering::Release);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Millis {
        self.now.load(Ordering::Acquire)
    }

    fn wall(&self) -> Millis {
        self.wall_offset + self.now()
    }
}
