use super::{ResourcePriorities, resource_key};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

// A fixed unit keeps semaphore permits small while bounding overlapping
// decoder, resize, and output buffers across all email documents.
const DECODE_UNIT_BYTES: u64 = 1024 * 1024;
const DECODE_BUDGET_UNITS: u32 = 128;

pub(super) struct Scheduler {
    state: Mutex<State>,
    changed: Notify,
    decode_bytes: Arc<Semaphore>,
    decode_budget_units: u32,
    limit: usize,
    remote_limit: usize,
}
#[derive(Default)]
struct State {
    next: u64,
    active: usize,
    remote_active: usize,
    waiting: Vec<Waiting>,
}
struct Waiting {
    ticket: u64,
    embedded: bool,
    key: (usize, u64),
    priorities: ResourcePriorities,
}

pub(super) struct Slot {
    scheduler: Arc<Scheduler>,
    ticket: u64,
    embedded: bool,
    active: bool,
}

pub(super) fn shared() -> Arc<Scheduler> {
    static INSTANCE: OnceLock<Arc<Scheduler>> = OnceLock::new();
    INSTANCE.get_or_init(|| Scheduler::new(4, 3)).clone()
}

impl Scheduler {
    pub(super) fn new(limit: usize, remote_limit: usize) -> Arc<Self> {
        Self::with_decode_budget(limit, remote_limit, DECODE_BUDGET_UNITS)
    }

    fn with_decode_budget(
        limit: usize,
        remote_limit: usize,
        decode_budget_units: u32,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            changed: Notify::new(),
            decode_bytes: Arc::new(Semaphore::new(decode_budget_units as usize)),
            decode_budget_units,
            limit,
            remote_limit,
        })
    }

    pub(super) async fn acquire_decode_bytes(
        &self,
        estimated_bytes: u64,
    ) -> Option<OwnedSemaphorePermit> {
        let units = estimated_bytes.div_ceil(DECODE_UNIT_BYTES).max(1);
        let units = u32::try_from(units).ok()?;
        if units > self.decode_budget_units {
            return None;
        }
        self.decode_bytes
            .clone()
            .acquire_many_owned(units)
            .await
            .ok()
    }
    pub(super) fn reprioritize(&self) {
        self.changed.notify_waiters();
    }

    pub(super) async fn acquire(
        self: Arc<Self>,
        doc: usize,
        url: &str,
        embedded: bool,
        priorities: ResourcePriorities,
    ) -> Slot {
        let ticket = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ticket = state.next;
            state.next = state.next.wrapping_add(1);
            state.waiting.push(Waiting {
                ticket,
                embedded,
                key: resource_key(doc, url),
                priorities,
            });
            ticket
        };
        let mut slot = Slot {
            scheduler: self.clone(),
            ticket,
            embedded,
            active: false,
        };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let best = state
                    .waiting
                    .iter()
                    .filter(|item| item.embedded || state.remote_active < self.remote_limit)
                    .min_by_key(|item| {
                        let visible = item
                            .priorities
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .contains(&item.key);
                        (
                            if item.embedded {
                                0
                            } else if visible {
                                1
                            } else {
                                2
                            },
                            item.ticket,
                        )
                    })
                    .map(|item| item.ticket);
                if state.active < self.limit && best == Some(ticket) {
                    state.waiting.retain(|item| item.ticket != ticket);
                    state.active += 1;
                    state.remote_active += usize::from(!embedded);
                    slot.active = true;
                    self.changed.notify_waiters();
                    return slot;
                }
            }
            changed.await;
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut state = self
            .scheduler
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.active {
            state.active -= 1;
            state.remote_active -= usize::from(!self.embedded);
        } else {
            state.waiting.retain(|item| item.ticket != self.ticket);
        }
        drop(state);
        self.scheduler.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelling_a_waiter_releases_its_queue_entry() {
        let scheduler = Scheduler::new(1, 1);
        let priorities = ResourcePriorities::default();
        let held = scheduler
            .clone()
            .acquire(1, "held", false, priorities.clone())
            .await;
        let mut waiting = Box::pin(scheduler.clone().acquire(
            1,
            "cancelled",
            false,
            priorities.clone(),
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut waiting)
                .await
                .is_err()
        );
        assert_eq!(scheduler.state.lock().unwrap().waiting.len(), 1);
        drop(waiting);
        assert!(scheduler.state.lock().unwrap().waiting.is_empty());
        drop(held);
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            scheduler.clone().acquire(2, "next", false, priorities),
        )
        .await
        .unwrap();
        drop(next);
        assert_eq!(scheduler.state.lock().unwrap().active, 0);
    }

    #[tokio::test]
    async fn visible_images_overtake_queued_remote_images_and_cancel_cleanly() {
        let scheduler = Scheduler::new(1, 1);
        let priorities = ResourcePriorities::default();
        let held = scheduler
            .clone()
            .acquire(1, "held", false, priorities.clone())
            .await;
        let background = scheduler
            .clone()
            .acquire(1, "background", false, priorities.clone());
        let visible = scheduler
            .clone()
            .acquire(1, "visible", false, priorities.clone());
        tokio::pin!(background, visible);
        // Poll both into the queue before changing priority.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut background)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut visible)
                .await
                .is_err()
        );
        priorities
            .lock()
            .unwrap()
            .insert(resource_key(1, "visible"));
        scheduler.reprioritize();
        drop(held);
        let visible = tokio::time::timeout(std::time::Duration::from_secs(1), &mut visible)
            .await
            .unwrap();
        assert_eq!(scheduler.state.lock().unwrap().active, 1);
        drop(visible);
        drop(background.await);
        assert!(scheduler.state.lock().unwrap().waiting.is_empty());
        assert_eq!(scheduler.state.lock().unwrap().active, 0);
    }

    #[tokio::test]
    async fn remote_downloads_leave_an_embedded_slot_available() {
        let scheduler = Scheduler::new(2, 1);
        let priorities = ResourcePriorities::default();
        let remote = scheduler
            .clone()
            .acquire(1, "remote", false, priorities.clone())
            .await;
        let queued = scheduler
            .clone()
            .acquire(1, "queued", false, priorities.clone());
        tokio::pin!(queued);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut queued)
                .await
                .is_err()
        );
        let local = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            scheduler.clone().acquire(1, "local", true, priorities),
        )
        .await
        .unwrap();
        assert_eq!(scheduler.state.lock().unwrap().active, 2);
        drop(remote);
        drop(local);
        drop(queued.await);
    }

    #[tokio::test]
    async fn decode_byte_budget_waits_and_releases_after_work() {
        let scheduler = Scheduler::with_decode_budget(4, 3, 4);
        let held = scheduler
            .acquire_decode_bytes(3 * DECODE_UNIT_BYTES)
            .await
            .unwrap();
        let mut waiting = Box::pin(scheduler.acquire_decode_bytes(2 * DECODE_UNIT_BYTES));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut waiting)
                .await
                .is_err()
        );
        // Tokio reserves the spare unit for the first queued waiter.
        assert_eq!(scheduler.decode_bytes.available_permits(), 0);
        drop(held);
        let acquired = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scheduler.decode_bytes.available_permits(), 2);
        drop(acquired);
        assert_eq!(scheduler.decode_bytes.available_permits(), 4);
    }

    #[tokio::test]
    async fn oversized_and_cancelled_decode_waits_leave_budget_available() {
        let scheduler = Scheduler::with_decode_budget(4, 3, 2);
        assert!(
            scheduler
                .acquire_decode_bytes(3 * DECODE_UNIT_BYTES)
                .await
                .is_none()
        );
        let held = scheduler
            .acquire_decode_bytes(2 * DECODE_UNIT_BYTES)
            .await
            .unwrap();
        let mut waiting = Box::pin(scheduler.acquire_decode_bytes(DECODE_UNIT_BYTES));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut waiting)
                .await
                .is_err()
        );
        drop(waiting);
        drop(held);
        assert_eq!(scheduler.decode_bytes.available_permits(), 2);
        let next = scheduler
            .acquire_decode_bytes(DECODE_UNIT_BYTES)
            .await
            .unwrap();
        drop(next);
    }
}
