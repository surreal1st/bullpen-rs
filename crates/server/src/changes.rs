//! One event bus for "something changed". Port of `src/server/changes.ts`.
//!
//! 🔴 The TS original is a single module-level `EventEmitter` - a process
//! singleton every route and every run shares. `cargo test` runs many test
//! functions in one process (the default is one thread per test, same
//! statics), so a real global here would let one test's `touch` calls leak
//! into another's running assertions - exactly what S1-05's acceptance case
//! 4 "counts across a gate, not merely contains" would go flaky on. So this
//! is an instance a `RunManager` owns rather than a process-wide static;
//! `AppState`/`RunManager` construction is where "one bus per app" already
//! lives, same shape as `AppState.db`.

use std::sync::{Arc, Mutex, PoisonError};

/// "working" is the working indicator at the bottom of the conversation, and
/// it is its own kind rather than another reason to touch "roster" on
/// purpose - a roster touch makes an open room re-read its entire message
/// list, and a run emits several of these a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Roster,
    Approvals,
    Questions,
    Working,
    Memory,
}

impl ChangeKind {
    /// The wire value `GET /api/events` puts in a `{"type":"change","kind":…}`
    /// frame. Lowercase, matching the TS union's own string literals.
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeKind::Roster => "roster",
            ChangeKind::Approvals => "approvals",
            ChangeKind::Questions => "questions",
            ChangeKind::Working => "working",
            ChangeKind::Memory => "memory",
        }
    }
}

type Listener = Box<dyn Fn(ChangeKind) + Send + Sync>;

struct Entry {
    id: u64,
    listener: Listener,
}

struct Inner {
    next_id: u64,
    listeners: Vec<Entry>,
}

/// A change bus. Cheap to clone - clones share the same listener list.
#[derive(Clone)]
pub struct ChangeBus(Arc<Mutex<Inner>>);

impl ChangeBus {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Inner {
            next_id: 0,
            listeners: Vec::new(),
        })))
    }

    /// Records that something of this kind changed. Fire and forget - calls
    /// every listener synchronously, undebounced, same as the TS `touch`.
    pub fn touch(&self, kind: ChangeKind) {
        let inner = self.0.lock().expect("change bus mutex poisoned");
        for entry in inner.listeners.iter() {
            (entry.listener)(kind);
        }
    }

    /// Registers `listener`, called once per touch, for the life of this
    /// bus - there is no way to unregister it short of dropping the whole
    /// `ChangeBus`. Right for a `RunManager`'s own subscribers (it owns the
    /// bus for its whole lifetime) or a test (drops the bus with it); wrong
    /// for anything that comes and goes, which wants `subscribe_scoped`
    /// instead - S1-F-04's B4 was exactly `GET /api/events` using this kind
    /// of subscribe on every reconnect, leaking one closure per connection
    /// forever.
    pub fn subscribe(&self, listener: impl Fn(ChangeKind) + Send + Sync + 'static) {
        self.subscribe_scoped(listener).leak();
    }

    /// Registers `listener`; returns a guard whose `Drop` unregisters it.
    /// `GET /api/events` holds this for the life of the SSE connection so a
    /// page reload / reconnect (the client retries every 3s) does not add a
    /// listener that outlives the stream (S1-F-04, B4).
    pub fn subscribe_scoped(
        &self,
        listener: impl Fn(ChangeKind) + Send + Sync + 'static,
    ) -> Subscription {
        let mut inner = self.0.lock().expect("change bus mutex poisoned");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.listeners.push(Entry {
            id,
            listener: Box::new(listener),
        });
        Subscription {
            bus: Arc::clone(&self.0),
            id,
        }
    }
}

impl Default for ChangeBus {
    fn default() -> Self {
        Self::new()
    }
}

/// The guard `subscribe_scoped` returns. Unregisters its listener on
/// `Drop` - hold it as long as the subscriber still cares about changes.
pub struct Subscription {
    bus: Arc<Mutex<Inner>>,
    id: u64,
}

impl Subscription {
    /// Keeps the listener registered for the life of the bus rather than
    /// unregistering it on drop - what the plain, unguarded `subscribe`
    /// calls internally so its own behaviour stays exactly what it always
    /// was.
    fn leak(self) {
        std::mem::forget(self);
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Never panic out of a `Drop` over a lock another thread might have
        // poisoned - a subscription unregistering is not the place to turn
        // one bad touch into a second one.
        let mut inner = self.bus.lock().unwrap_or_else(PoisonError::into_inner);
        inner.listeners.retain(|entry| entry.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn touch_reaches_every_subscriber_with_the_kind() {
        let bus = ChangeBus::new();
        let seen: Arc<StdMutex<Vec<ChangeKind>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        bus.subscribe(move |kind| seen_clone.lock().unwrap().push(kind));

        bus.touch(ChangeKind::Working);
        bus.touch(ChangeKind::Roster);

        assert_eq!(
            *seen.lock().unwrap(),
            vec![ChangeKind::Working, ChangeKind::Roster]
        );
    }
}
