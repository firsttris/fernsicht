//! Latest-frame-wins hand-off between two pipeline stages.
//!
//! A [`Slot`] holds at most one value. Putting a new value replaces the old
//! one and hands the old one back to the producer, so pre-allocated frame
//! buffers can be recycled instead of dropped. The consumer blocks until a
//! value is available. Latency never builds up: if the consumer is slow,
//! frames are skipped, not queued.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub struct Slot<T> {
    value: Mutex<SlotState<T>>,
    ready: Condvar,
    overwritten: AtomicU64,
}

struct SlotState<T> {
    value: Option<T>,
    closed: bool,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Slot<T> {
    pub const fn new() -> Self {
        Self {
            value: Mutex::new(SlotState {
                value: None,
                closed: false,
            }),
            ready: Condvar::new(),
            overwritten: AtomicU64::new(0),
        }
    }

    /// Stores `value`, returning the value it replaced (a skipped frame) so
    /// its buffer can be reused.
    pub fn put(&self, value: T) -> Option<T> {
        let mut state = self.value.lock().unwrap();
        let old = state.value.replace(value);
        drop(state);
        if old.is_some() {
            self.overwritten.fetch_add(1, Ordering::Relaxed);
        }
        self.ready.notify_one();
        old
    }

    /// Takes the current value without waiting.
    pub fn try_take(&self) -> Option<T> {
        self.value.lock().unwrap().value.take()
    }

    /// Blocks until a value is available or the slot is closed.
    pub fn take(&self) -> Option<T> {
        let mut state = self.value.lock().unwrap();
        loop {
            if let Some(v) = state.value.take() {
                return Some(v);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    /// Like [`take`](Self::take) but gives up after `timeout`.
    pub fn take_timeout(&self, timeout: Duration) -> Option<T> {
        let state = self.value.lock().unwrap();
        let (mut state, _) = self
            .ready
            .wait_timeout_while(state, timeout, |s| s.value.is_none() && !s.closed)
            .unwrap();
        state.value.take()
    }

    /// Wakes the consumer; subsequent `take` calls return `None` once empty.
    pub fn close(&self) {
        self.value.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.value.lock().unwrap().closed
    }

    /// Number of values that were replaced before the consumer took them.
    pub fn overwritten(&self) -> u64 {
        self.overwritten.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn latest_value_wins() {
        let slot = Slot::new();
        assert_eq!(slot.put(1), None);
        assert_eq!(slot.put(2), Some(1));
        assert_eq!(slot.put(3), Some(2));
        assert_eq!(slot.take(), Some(3));
        assert_eq!(slot.try_take(), None);
        assert_eq!(slot.overwritten(), 2);
    }

    #[test]
    fn take_blocks_until_put() {
        let slot = Arc::new(Slot::new());
        let consumer = {
            let slot = slot.clone();
            thread::spawn(move || slot.take())
        };
        thread::sleep(Duration::from_millis(20));
        slot.put(42u32);
        assert_eq!(consumer.join().unwrap(), Some(42));
    }

    #[test]
    fn close_wakes_consumer() {
        let slot = Arc::new(Slot::<u32>::new());
        let consumer = {
            let slot = slot.clone();
            thread::spawn(move || slot.take())
        };
        thread::sleep(Duration::from_millis(20));
        slot.close();
        assert_eq!(consumer.join().unwrap(), None);
    }

    #[test]
    fn take_timeout_expires() {
        let slot = Slot::<u32>::new();
        assert_eq!(slot.take_timeout(Duration::from_millis(5)), None);
        slot.put(7);
        assert_eq!(slot.take_timeout(Duration::from_millis(5)), Some(7));
    }
}
