//! Two independent fan-out mechanisms sitting on top of the same
//! `robocol::Event` stream: `WaiterRegistry` resolves exactly one
//! request/response call per registration and then forgets it;
//! `Subscribers` keeps broadcasting to every `watch` connection until it
//! disconnects.

use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};

use robocol::client::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaiterKind {
    OpModeList,
    OpModeInited,
    OpModeRunning,
    ActiveConfiguration,
    ConfigurationList,
    Configuration,
    UserDeviceList,
    ScanResult,
    /// Cannot be payload-matched: `Event::LynxModules` carries only the RC's
    /// raw response, not the serial it was requested for, so a `LynxModules`
    /// waiter is resolved by the first matching-kind event (register it with
    /// a `|_| true` predicate).
    LynxModules,
}

pub fn waiter_kind_of(event: &Event) -> Option<WaiterKind> {
    match event {
        Event::OpModeList(_) => Some(WaiterKind::OpModeList),
        Event::OpModeInited(_) => Some(WaiterKind::OpModeInited),
        Event::OpModeRunning(_) => Some(WaiterKind::OpModeRunning),
        Event::ActiveConfiguration(_) => Some(WaiterKind::ActiveConfiguration),
        Event::ConfigurationList(_) => Some(WaiterKind::ConfigurationList),
        Event::Configuration(_) => Some(WaiterKind::Configuration),
        Event::UserDeviceList(_) => Some(WaiterKind::UserDeviceList),
        Event::ScanResult(_) => Some(WaiterKind::ScanResult),
        Event::LynxModules(_) => Some(WaiterKind::LynxModules),
        _ => None,
    }
}

/// Decides whether a same-kind event is the one a particular waiter asked
/// for (e.g. `OpModeInited(n)` with `n == requested_name`), so an unsolicited
/// or stale event of the right kind can't resolve the wrong request.
pub type WaiterPredicate = Box<dyn Fn(&Event) -> bool + Send>;

pub struct WaiterRegistry {
    waiters: Vec<(WaiterKind, WaiterPredicate, Sender<Event>)>,
}

impl WaiterRegistry {
    pub fn new() -> Self {
        WaiterRegistry {
            waiters: Vec::new(),
        }
    }

    pub fn register(&mut self, kind: WaiterKind, pred: WaiterPredicate) -> Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        self.waiters.push((kind, pred, tx));
        rx
    }

    /// Delivers `event` to every waiter registered for its kind whose
    /// predicate accepts it, then drops them — each registration is resolved
    /// (or forgotten, on send failure) exactly once. Waiters of other kinds,
    /// or whose predicate rejects the payload, are left untouched.
    pub fn resolve(&mut self, event: &Event) {
        let Some(kind) = waiter_kind_of(event) else {
            return;
        };
        self.waiters.retain(|(k, pred, tx)| {
            if *k == kind && pred(event) {
                let _ = tx.send(event.clone());
                false
            } else {
                true
            }
        });
    }
}

/// Per-subscriber queue depth. A stalled `ds_agent watch` reader must not
/// grow the daemon's memory without bound, so events past this are dropped
/// for that subscriber only.
const SUBSCRIBER_QUEUE: usize = 64;

pub struct Subscribers {
    list: Vec<SyncSender<Event>>,
}

impl Subscribers {
    pub fn new() -> Self {
        Subscribers { list: Vec::new() }
    }

    pub fn subscribe(&mut self) -> Receiver<Event> {
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
        self.list.push(tx);
        rx
    }

    /// Sends `event` to every live subscriber, dropping any whose receiver
    /// has gone away (their `watch` connection closed). A subscriber whose
    /// queue is full just misses this event; it is not pruned.
    pub fn broadcast(&mut self, event: &Event) {
        self.list.retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any() -> WaiterPredicate {
        Box::new(|_| true)
    }

    #[test]
    fn resolve_delivers_to_matching_kind_only() {
        let mut registry = WaiterRegistry::new();
        let opmode_rx = registry.register(WaiterKind::OpModeList, any());
        let config_rx = registry.register(WaiterKind::ActiveConfiguration, any());

        registry.resolve(&Event::OpModeList(vec![]));

        assert!(opmode_rx.try_recv().is_ok());
        assert!(config_rx.try_recv().is_err());
    }

    #[test]
    fn resolve_forgets_waiter_after_one_delivery() {
        let mut registry = WaiterRegistry::new();
        let rx = registry.register(WaiterKind::ScanResult, any());

        registry.resolve(&Event::ScanResult("first".into()));
        registry.resolve(&Event::ScanResult("second".into()));

        assert_eq!(rx.try_recv().unwrap(), Event::ScanResult("first".into()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn resolve_ignores_events_with_no_waiter_kind() {
        let mut registry = WaiterRegistry::new();
        let rx = registry.register(WaiterKind::OpModeList, any());
        registry.resolve(&Event::Disconnected);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dropped_receiver_is_pruned_on_next_matching_event() {
        let mut registry = WaiterRegistry::new();
        drop(registry.register(WaiterKind::ScanResult, any()));
        // Resolving with a dropped receiver must not panic, and the dead
        // entry must be gone afterward (checked indirectly: a second
        // resolve with a fresh waiter only sees its own event).
        registry.resolve(&Event::ScanResult("ignored".into()));
        let rx = registry.register(WaiterKind::ScanResult, any());
        registry.resolve(&Event::ScanResult("seen".into()));
        assert_eq!(rx.try_recv().unwrap(), Event::ScanResult("seen".into()));
    }

    #[test]
    fn predicate_rejects_same_kind_event_with_other_payload() {
        let mut registry = WaiterRegistry::new();
        let rx = registry.register(
            WaiterKind::OpModeInited,
            Box::new(|e| matches!(e, Event::OpModeInited(n) if n == "Foo")),
        );

        // The RC's unsolicited "$Stop$Robot$" must not resolve an `init Foo`.
        registry.resolve(&Event::OpModeInited("$Stop$Robot$".into()));
        assert!(rx.try_recv().is_err());
        assert_eq!(registry.waiters.len(), 1);

        registry.resolve(&Event::OpModeInited("Foo".into()));
        assert_eq!(rx.try_recv().unwrap(), Event::OpModeInited("Foo".into()));
        assert!(registry.waiters.is_empty());
    }

    #[test]
    fn broadcast_reaches_every_subscriber() {
        let mut subs = Subscribers::new();
        let a = subs.subscribe();
        let b = subs.subscribe();
        subs.broadcast(&Event::Disconnected);
        assert_eq!(a.try_recv().unwrap(), Event::Disconnected);
        assert_eq!(b.try_recv().unwrap(), Event::Disconnected);
    }

    #[test]
    fn broadcast_drops_disconnected_subscribers() {
        let mut subs = Subscribers::new();
        {
            let _rx = subs.subscribe(); // dropped immediately
        }
        let live = subs.subscribe();
        subs.broadcast(&Event::Disconnected);
        assert_eq!(live.try_recv().unwrap(), Event::Disconnected);
        assert_eq!(subs.list.len(), 1);
    }

    #[test]
    fn full_queue_drops_events_but_keeps_subscriber_while_disconnected_is_pruned() {
        let mut subs = Subscribers::new();
        let slow = subs.subscribe(); // never drained
        {
            let _gone = subs.subscribe(); // dropped: must be pruned
        }
        assert_eq!(subs.list.len(), 2);

        // Overfill the slow subscriber's queue; broadcast must neither block
        // nor prune it.
        for _ in 0..(SUBSCRIBER_QUEUE + 10) {
            subs.broadcast(&Event::Disconnected);
        }
        assert_eq!(subs.list.len(), 1, "full queue must not prune; dead one must");

        // Exactly the queue depth got through; the excess was dropped.
        let mut received = 0;
        while slow.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, SUBSCRIBER_QUEUE);

        // Once drained, the subscriber receives again.
        subs.broadcast(&Event::Disconnected);
        assert_eq!(slow.try_recv().unwrap(), Event::Disconnected);
    }
}
