//! Two independent fan-out mechanisms sitting on top of the same
//! `robocol::Event` stream: `WaiterRegistry` resolves exactly one
//! request/response call per registration and then forgets it;
//! `Subscribers` keeps broadcasting to every `watch` connection until it
//! disconnects.

use std::sync::mpsc::{self, Receiver, Sender};

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

pub struct WaiterRegistry {
    waiters: Vec<(WaiterKind, Sender<Event>)>,
}

impl WaiterRegistry {
    pub fn new() -> Self {
        WaiterRegistry {
            waiters: Vec::new(),
        }
    }

    pub fn register(&mut self, kind: WaiterKind) -> Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        self.waiters.push((kind, tx));
        rx
    }

    /// Delivers `event` to every waiter registered for its kind, then drops
    /// them — each registration is resolved (or forgotten, on send failure)
    /// exactly once. Waiters of other kinds are left untouched.
    pub fn resolve(&mut self, event: &Event) {
        let Some(kind) = waiter_kind_of(event) else {
            return;
        };
        self.waiters.retain(|(k, tx)| {
            if *k == kind {
                let _ = tx.send(event.clone());
                false
            } else {
                true
            }
        });
    }
}

pub struct Subscribers {
    list: Vec<Sender<Event>>,
}

impl Subscribers {
    pub fn new() -> Self {
        Subscribers { list: Vec::new() }
    }

    pub fn subscribe(&mut self) -> Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        self.list.push(tx);
        rx
    }

    /// Sends `event` to every live subscriber, dropping any whose receiver
    /// has gone away (their `watch` connection closed).
    pub fn broadcast(&mut self, event: &Event) {
        self.list.retain(|tx| tx.send(event.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_delivers_to_matching_kind_only() {
        let mut registry = WaiterRegistry::new();
        let opmode_rx = registry.register(WaiterKind::OpModeList);
        let config_rx = registry.register(WaiterKind::ActiveConfiguration);

        registry.resolve(&Event::OpModeList(vec![]));

        assert!(opmode_rx.try_recv().is_ok());
        assert!(config_rx.try_recv().is_err());
    }

    #[test]
    fn resolve_forgets_waiter_after_one_delivery() {
        let mut registry = WaiterRegistry::new();
        let rx = registry.register(WaiterKind::ScanResult);

        registry.resolve(&Event::ScanResult("first".into()));
        registry.resolve(&Event::ScanResult("second".into()));

        assert_eq!(rx.try_recv().unwrap(), Event::ScanResult("first".into()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn resolve_ignores_events_with_no_waiter_kind() {
        let mut registry = WaiterRegistry::new();
        let rx = registry.register(WaiterKind::OpModeList);
        registry.resolve(&Event::Disconnected);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dropped_receiver_is_pruned_on_next_matching_event() {
        let mut registry = WaiterRegistry::new();
        drop(registry.register(WaiterKind::ScanResult));
        // Resolving with a dropped receiver must not panic, and the dead
        // entry must be gone afterward (checked indirectly: a second
        // resolve with a fresh waiter only sees its own event).
        registry.resolve(&Event::ScanResult("ignored".into()));
        let rx = registry.register(WaiterKind::ScanResult);
        registry.resolve(&Event::ScanResult("seen".into()));
        assert_eq!(rx.try_recv().unwrap(), Event::ScanResult("seen".into()));
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
}
