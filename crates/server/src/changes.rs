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

use std::sync::{Arc, Mutex};

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
}

type Listener = Box<dyn Fn(ChangeKind) + Send + Sync>;

/// A change bus. Cheap to clone - clones share the same listener list.
#[derive(Clone)]
pub struct ChangeBus(Arc<Mutex<Vec<Listener>>>);

impl ChangeBus {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    /// Records that something of this kind changed. Fire and forget - calls
    /// every listener synchronously, undebounced, same as the TS `touch`.
    pub fn touch(&self, kind: ChangeKind) {
        let listeners = self.0.lock().expect("change bus mutex poisoned");
        for listener in listeners.iter() {
            listener(kind);
        }
    }

    /// Registers `listener`, called once per touch. Lives as long as this
    /// bus does - there is no unsubscribe handle, unlike the TS `subscribe`,
    /// because every caller here is either a `RunManager` (subscribes for
    /// its own whole lifetime) or a test (drops the bus with the manager).
    pub fn subscribe(&self, listener: impl Fn(ChangeKind) + Send + Sync + 'static) {
        self.0
            .lock()
            .expect("change bus mutex poisoned")
            .push(Box::new(listener));
    }
}

impl Default for ChangeBus {
    fn default() -> Self {
        Self::new()
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
