//! The run's table slots: a semaphore whose size changes live.
//!
//! Lowering the limit never stops a running table: free permits are
//! withdrawn at once and, for the rest, a "debt" is recorded; a running
//! table that ends pays the debt (its permit is forgotten instead of
//! released). Raising the limit cancels debt first and only then adds
//! permits. Invariant: held + free = target + debt.

use crate::lock;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Most tables a run copies at once.
pub const MAX_PARALLEL: usize = 32;

pub(crate) struct Slots {
    sem: Arc<Semaphore>,
    state: Mutex<SlotState>,
}

struct SlotState {
    target: usize,
    debt: usize,
}

/// A running table's slot; released (or used to pay debt) on drop.
pub(crate) struct Slot {
    permit: Option<OwnedSemaphorePermit>,
    slots: Arc<Slots>,
}

impl Slots {
    pub fn new(n: usize) -> Arc<Self> {
        let n = n.clamp(1, MAX_PARALLEL);
        Arc::new(Slots { sem: Arc::new(Semaphore::new(n)), state: Mutex::new(SlotState { target: n, debt: 0 }) })
    }

    pub fn target(&self) -> usize {
        lock(&self.state).target
    }

    /// Change the limit; running tables keep going.
    pub fn set(&self, n: usize) {
        let n = n.clamp(1, MAX_PARALLEL);
        let mut st = lock(&self.state);
        if n > st.target {
            let add = n - st.target;
            let pay = add.min(st.debt);
            st.debt -= pay;
            self.sem.add_permits(add - pay);
        } else if n < st.target {
            let remove = st.target - n;
            let forgot = self.sem.forget_permits(remove);
            st.debt += remove - forgot;
        }
        st.target = n;
    }

    /// Wait for a free slot.
    pub async fn acquire(self: &Arc<Self>) -> Slot {
        // The semaphore is never closed.
        let permit = self.sem.clone().acquire_owned().await.ok();
        Slot { permit, slots: self.clone() }
    }

    #[cfg(test)]
    fn try_acquire(self: &Arc<Self>) -> Option<Slot> {
        let permit = self.sem.clone().try_acquire_owned().ok()?;
        Some(Slot { permit: Some(permit), slots: self.clone() })
    }

    #[cfg(test)]
    fn free(&self) -> usize {
        self.sem.available_permits()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut st = lock(&self.slots.state);
        if let Some(p) = self.permit.take() {
            if st.debt > 0 {
                st.debt -= 1;
                p.forget();
            }
            // Otherwise `p` drops here, under the lock: released.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_waits_for_running_tables() {
        let s = Slots::new(3);
        let a = s.try_acquire().unwrap();
        let b = s.try_acquire().unwrap();
        let c = s.try_acquire().unwrap();
        s.set(1);
        assert!(s.try_acquire().is_none());
        drop(a);
        drop(b);
        // Two tables ended and paid the debt: still nothing free.
        assert!(s.try_acquire().is_none());
        drop(c);
        assert_eq!(s.free(), 1);
    }

    #[test]
    fn two_three_two_three_never_starts_a_fourth() {
        let s = Slots::new(2);
        let a = s.try_acquire().unwrap();
        let b = s.try_acquire().unwrap();
        s.set(3);
        let c = s.try_acquire().unwrap();
        s.set(2);
        s.set(3);
        assert!(s.try_acquire().is_none(), "a 4th table would start");
        drop(a);
        // The limit is 3 again and 2 run: one starts.
        let d = s.try_acquire().unwrap();
        assert!(s.try_acquire().is_none());
        drop((b, c, d));
        assert_eq!(s.free(), 3);
    }

    #[test]
    fn raising_adds_free_slots() {
        let s = Slots::new(1);
        let _a = s.try_acquire().unwrap();
        s.set(4);
        assert_eq!(s.free(), 3);
        s.set(2);
        assert_eq!(s.free(), 1);
        assert_eq!(s.target(), 2);
    }
}
