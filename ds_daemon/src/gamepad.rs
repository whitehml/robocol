//! Who may drive each gamepad, and how gamepad input reaches the RC.
//!
//! Each pad has an owner: the operator (`ds_cli`), the agent (`ds_agent`),
//! or nobody. The operator's claim always wins; an agent's claim on a pad
//! the operator holds is refused. Every claim and release sends a neutral
//! packet for that pad before anyone's next input, so nobody inherits a
//! stick someone else left pushed, and re-claiming is how to recover one.
//!
//! Input goes out as a pulse: repeated at 50 Hz for its duration, then a
//! short neutral tail. Gamepad packets are not acknowledged, so a single
//! datagram could be lost, silently skipping the command or leaving the pad
//! deflected.

use std::sync::Mutex;
use std::time::Duration;

use ds_proto::{GamepadInput, Pad, Role};
use robocol::packets::Gamepad;
use serde_json::{Value, json};

const PULSE_PERIOD_MS: u64 = 20;
/// Time between the packets of a pulse: 50 Hz, as Deck Station sends.
pub const PULSE_PERIOD: Duration = Duration::from_millis(PULSE_PERIOD_MS);
/// Pulse length when a `gamepad` request names none.
pub const DEFAULT_PULSE_MS: u64 = 300;
/// The longest pulse one request may ask for.
pub const MAX_PULSE_MS: u64 = 10_000;
/// Neutral packets sent after a pulse.
pub const TAIL_PACKETS: u32 = 5;

#[derive(Debug, Default, Clone, Copy)]
struct Slot {
    owner: Option<Role>,
    /// Moves on every claim, release and new pulse of this pad. A running
    /// pulse that sees it move stops.
    generation: u64,
}

/// Pad ownership. Pure bookkeeping: the functions below send the packets.
#[derive(Debug, Default)]
pub struct Arbiter {
    slots: [Slot; 2],
}

/// One pad changing hands, as announced on `watch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerChange {
    pub pad: Pad,
    pub owner: Option<Role>,
    pub previous: Option<Role>,
}

impl OwnerChange {
    pub fn to_json(self) -> Value {
        json!({
            "event": "gamepad_owner",
            "user": self.pad.user(),
            "owner": self.owner,
            "previous": self.previous,
        })
    }
}

/// Why a claim or a pulse was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The operator holds the pad; the agent cannot have it.
    Preempted(Pad),
    /// The caller does not hold the pad.
    NotClaimed(Pad),
}

impl Refusal {
    pub fn to_json(self) -> Value {
        match self {
            Refusal::Preempted(pad) => json!({
                "ok": false,
                "error": "preempted",
                "message": format!("the operator holds {}; wait for it to be released", pad.name()),
            }),
            Refusal::NotClaimed(pad) => json!({
                "ok": false,
                "error": "not_claimed",
                "message": format!("you do not hold {0}; run `claim {0}` first", pad.name()),
            }),
        }
    }
}

/// Where a running pulse stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseStatus {
    Current,
    /// A newer pulse by the same owner has taken over.
    Superseded,
    /// The pad now belongs to someone else, or to nobody.
    Lost(Option<Role>),
}

impl Arbiter {
    fn slot(&self, pad: Pad) -> &Slot {
        &self.slots[usize::from(pad.user() - 1)]
    }

    fn slot_mut(&mut self, pad: Pad) -> &mut Slot {
        &mut self.slots[usize::from(pad.user() - 1)]
    }

    pub fn owner(&self, pad: Pad) -> Option<Role> {
        self.slot(pad).owner
    }

    /// The pads `role` holds, in pad order.
    pub fn held_by(&self, role: Role) -> Vec<Pad> {
        Pad::ALL
            .into_iter()
            .filter(|pad| self.owner(*pad) == Some(role))
            .collect()
    }

    /// Gives `pads` to `role`. The operator always succeeds; an agent's claim
    /// fails as a whole, changing nothing, if the operator holds any of them.
    /// Every claimed pad's generation moves, stopping any pulse on it. The
    /// result lists only the pads whose owner changed.
    pub fn claim(&mut self, role: Role, pads: &[Pad]) -> Result<Vec<OwnerChange>, Refusal> {
        if role == Role::Agent
            && let Some(&pad) = pads
                .iter()
                .find(|pad| self.owner(**pad) == Some(Role::Operator))
        {
            return Err(Refusal::Preempted(pad));
        }
        let mut changes = Vec::new();
        for &pad in pads {
            let slot = self.slot_mut(pad);
            slot.generation += 1;
            if slot.owner != Some(role) {
                changes.push(OwnerChange {
                    pad,
                    owner: Some(role),
                    previous: slot.owner,
                });
                slot.owner = Some(role);
            }
        }
        Ok(changes)
    }

    /// Gives back those of `pads` that `role` holds, skipping the rest.
    pub fn release(&mut self, role: Role, pads: &[Pad]) -> Vec<OwnerChange> {
        let mut changes = Vec::new();
        for &pad in pads {
            let slot = self.slot_mut(pad);
            if slot.owner == Some(role) {
                slot.owner = None;
                slot.generation += 1;
                changes.push(OwnerChange {
                    pad,
                    owner: None,
                    previous: Some(role),
                });
            }
        }
        changes
    }

    /// Starts a pulse by `role` on `pad`, superseding any earlier one, and
    /// returns the generation the pulse must keep seeing.
    pub fn begin_pulse(&mut self, role: Role, pad: Pad) -> Result<u64, Refusal> {
        let slot = self.slot_mut(pad);
        match slot.owner {
            Some(owner) if owner == role => {
                slot.generation += 1;
                Ok(slot.generation)
            }
            Some(Role::Operator) => Err(Refusal::Preempted(pad)),
            _ => Err(Refusal::NotClaimed(pad)),
        }
    }

    pub fn pulse_status(&self, role: Role, pad: Pad, generation: u64) -> PulseStatus {
        let slot = self.slot(pad);
        if slot.owner != Some(role) {
            PulseStatus::Lost(slot.owner)
        } else if slot.generation != generation {
            PulseStatus::Superseded
        } else {
            PulseStatus::Current
        }
    }

    /// `{"gamepad1": "operator"|"agent"|null, "gamepad2": ...}` for `status`.
    pub fn to_json(&self) -> Value {
        json!({
            "gamepad1": self.owner(Pad::Gamepad1),
            "gamepad2": self.owner(Pad::Gamepad2),
        })
    }
}

/// Every stick, trigger and button at rest, addressed to `pad`. Never the
/// bare `Gamepad::default()`, whose `user` is always 1.
pub fn neutral(pad: Pad) -> Gamepad {
    Gamepad {
        user: pad.user(),
        ..Gamepad::default()
    }
}

/// The packet for `input` on `pad`; any field left unset is zero / released.
pub fn to_packet(input: &GamepadInput, pad: Pad) -> Gamepad {
    Gamepad {
        user: pad.user(),
        left_stick_x: input.left_stick_x.unwrap_or(0.0),
        left_stick_y: input.left_stick_y.unwrap_or(0.0),
        right_stick_x: input.right_stick_x.unwrap_or(0.0),
        right_stick_y: input.right_stick_y.unwrap_or(0.0),
        left_trigger: input.left_trigger.unwrap_or(0.0),
        right_trigger: input.right_trigger.unwrap_or(0.0),
        dpad_up: input.dpad_up.unwrap_or(false),
        dpad_down: input.dpad_down.unwrap_or(false),
        dpad_left: input.dpad_left.unwrap_or(false),
        dpad_right: input.dpad_right.unwrap_or(false),
        a: input.a.unwrap_or(false),
        b: input.b.unwrap_or(false),
        x: input.x.unwrap_or(false),
        y: input.y.unwrap_or(false),
        start: input.start.unwrap_or(false),
        back: input.back.unwrap_or(false),
        left_bumper: input.left_bumper.unwrap_or(false),
        right_bumper: input.right_bumper.unwrap_or(false),
        left_stick_button: input.left_stick_button.unwrap_or(false),
        right_stick_button: input.right_stick_button.unwrap_or(false),
        ..Gamepad::default()
    }
}

/// `Arbiter::claim`, sending each claimed pad's neutral packet before the
/// lock is released, so it reaches the RC ahead of the new owner's input.
pub fn claim(
    arbiter: &Mutex<Arbiter>,
    role: Role,
    pads: &[Pad],
    mut send: impl FnMut(&Gamepad),
) -> Result<Vec<OwnerChange>, Refusal> {
    let mut arbiter = arbiter.lock().expect("arbiter mutex poisoned");
    let changes = arbiter.claim(role, pads)?;
    for &pad in pads {
        send(&neutral(pad));
    }
    Ok(changes)
}

/// `Arbiter::release`, sending a neutral packet for each released pad.
pub fn release(
    arbiter: &Mutex<Arbiter>,
    role: Role,
    pads: &[Pad],
    mut send: impl FnMut(&Gamepad),
) -> Vec<OwnerChange> {
    let mut arbiter = arbiter.lock().expect("arbiter mutex poisoned");
    let changes = arbiter.release(role, pads);
    for change in &changes {
        send(&neutral(change.pad));
    }
    changes
}

/// How a pulse ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseEnd {
    /// Ran for its whole duration and returned the pad to neutral.
    Completed { packets: u32 },
    /// A newer pulse by the same owner took over; that one sends the tail.
    Superseded { packets: u32 },
    /// The pad changed hands mid-pulse. The claim or release that took it
    /// already sent a neutral packet.
    Lost { owner: Option<Role>, packets: u32 },
}

/// Sends `packet` to `pad` every `PULSE_PERIOD` for `duration_ms` (at least
/// once), then `TAIL_PACKETS` neutral packets. Each packet is checked and
/// sent under the arbiter lock, so none can follow the neutral packet of a
/// claim that took the pad. No lock is held while sleeping.
pub fn pulse(
    arbiter: &Mutex<Arbiter>,
    role: Role,
    pad: Pad,
    packet: &Gamepad,
    duration_ms: u64,
    mut send: impl FnMut(&Gamepad),
) -> Result<PulseEnd, Refusal> {
    let generation = arbiter
        .lock()
        .expect("arbiter mutex poisoned")
        .begin_pulse(role, pad)?;
    let ticks = duration_ms.div_ceil(PULSE_PERIOD_MS).max(1);
    let mut packets = 0;
    for _ in 0..ticks {
        {
            let arbiter = arbiter.lock().expect("arbiter mutex poisoned");
            match arbiter.pulse_status(role, pad, generation) {
                PulseStatus::Current => send(packet),
                PulseStatus::Superseded => return Ok(PulseEnd::Superseded { packets }),
                PulseStatus::Lost(owner) => return Ok(PulseEnd::Lost { owner, packets }),
            }
        }
        packets += 1;
        std::thread::sleep(PULSE_PERIOD);
    }
    let rest = neutral(pad);
    for _ in 0..TAIL_PACKETS {
        {
            let arbiter = arbiter.lock().expect("arbiter mutex poisoned");
            if arbiter.pulse_status(role, pad, generation) != PulseStatus::Current {
                break;
            }
            send(&rest);
        }
        std::thread::sleep(PULSE_PERIOD);
    }
    Ok(PulseEnd::Completed { packets })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn stick(pad: Pad, y: f32) -> Gamepad {
        let input = GamepadInput {
            left_stick_y: Some(y),
            ..Default::default()
        };
        to_packet(&input, pad)
    }

    fn is_neutral(packet: &Gamepad) -> bool {
        *packet == neutral(Pad::from_user(packet.user).expect("user 1 or 2"))
    }

    #[test]
    fn neutral_addresses_its_own_pad() {
        assert_eq!(neutral(Pad::Gamepad2).user, 2);
        assert_eq!(neutral(Pad::Gamepad1).user, 1);
        assert_eq!(neutral(Pad::Gamepad2).left_stick_y, 0.0);
    }

    #[test]
    fn packets_carry_the_pad_and_the_input() {
        let input = GamepadInput {
            left_stick_y: Some(-0.5),
            a: Some(true),
            ..Default::default()
        };
        let packet = to_packet(&input, Pad::Gamepad2);
        assert_eq!(packet.user, 2);
        assert_eq!(packet.left_stick_y, -0.5);
        assert!(packet.a);
        assert_eq!(packet.right_stick_x, 0.0);
        assert!(!packet.b);
    }

    #[test]
    fn the_operator_preempts_the_agent() {
        let mut arbiter = Arbiter::default();
        assert_eq!(
            arbiter.claim(Role::Agent, &[Pad::Gamepad1]),
            Ok(vec![OwnerChange {
                pad: Pad::Gamepad1,
                owner: Some(Role::Agent),
                previous: None,
            }])
        );
        assert_eq!(
            arbiter.claim(Role::Operator, &[Pad::Gamepad1]),
            Ok(vec![OwnerChange {
                pad: Pad::Gamepad1,
                owner: Some(Role::Operator),
                previous: Some(Role::Agent),
            }])
        );
        assert_eq!(arbiter.owner(Pad::Gamepad1), Some(Role::Operator));
    }

    #[test]
    fn an_agent_claim_on_an_operator_pad_changes_nothing() {
        let mut arbiter = Arbiter::default();
        arbiter.claim(Role::Operator, &[Pad::Gamepad2]).unwrap();
        assert_eq!(
            arbiter.claim(Role::Agent, &Pad::ALL),
            Err(Refusal::Preempted(Pad::Gamepad2))
        );
        assert_eq!(arbiter.owner(Pad::Gamepad1), None);
        assert_eq!(arbiter.held_by(Role::Operator), vec![Pad::Gamepad2]);
    }

    #[test]
    fn every_claim_sends_a_neutral_packet_even_for_a_pad_already_held() {
        let arbiter = Mutex::new(Arbiter::default());
        let mut sent = Vec::new();
        claim(&arbiter, Role::Agent, &[Pad::Gamepad2], |g| {
            sent.push(g.clone())
        })
        .unwrap();
        let again = claim(&arbiter, Role::Agent, &[Pad::Gamepad2], |g| {
            sent.push(g.clone())
        })
        .unwrap();
        assert!(again.is_empty(), "re-claiming changes no owner");
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|g| g.user == 2 && is_neutral(g)));
    }

    #[test]
    fn release_returns_only_pads_you_hold() {
        let arbiter = Mutex::new(Arbiter::default());
        claim(&arbiter, Role::Agent, &[Pad::Gamepad1], |_| {}).unwrap();
        let mut sent = Vec::new();
        let changes = release(&arbiter, Role::Agent, &Pad::ALL, |g| sent.push(g.clone()));
        assert_eq!(
            changes,
            vec![OwnerChange {
                pad: Pad::Gamepad1,
                owner: None,
                previous: Some(Role::Agent),
            }]
        );
        assert_eq!(sent.len(), 1);
        assert!(sent[0].user == 1 && is_neutral(&sent[0]));
        assert!(
            release(&arbiter, Role::Operator, &Pad::ALL, |_| panic!(
                "nothing held"
            ))
            .is_empty()
        );
    }

    #[test]
    fn a_pulse_needs_the_pad() {
        let arbiter = Mutex::new(Arbiter::default());
        let packet = stick(Pad::Gamepad1, 0.5);
        assert_eq!(
            pulse(
                &arbiter,
                Role::Agent,
                Pad::Gamepad1,
                &packet,
                20,
                |_| panic!("sent unclaimed")
            ),
            Err(Refusal::NotClaimed(Pad::Gamepad1))
        );
        claim(&arbiter, Role::Operator, &[Pad::Gamepad1], |_| {}).unwrap();
        assert_eq!(
            pulse(
                &arbiter,
                Role::Agent,
                Pad::Gamepad1,
                &packet,
                20,
                |_| panic!("sent preempted")
            ),
            Err(Refusal::Preempted(Pad::Gamepad1))
        );
    }

    #[test]
    fn a_pulse_streams_its_input_then_a_neutral_tail() {
        let arbiter = Mutex::new(Arbiter::default());
        claim(&arbiter, Role::Agent, &[Pad::Gamepad2], |_| {}).unwrap();
        let mut sent = Vec::new();
        let packet = stick(Pad::Gamepad2, -0.5);
        let end = pulse(&arbiter, Role::Agent, Pad::Gamepad2, &packet, 60, |g| {
            sent.push(g.clone())
        });
        assert_eq!(end, Ok(PulseEnd::Completed { packets: 3 }));
        assert_eq!(sent.len(), 3 + TAIL_PACKETS as usize);
        assert!(sent[..3].iter().all(|g| *g == packet));
        assert!(sent[3..].iter().all(|g| g.user == 2 && is_neutral(g)));
    }

    #[test]
    fn a_zero_length_pulse_still_sends_once() {
        let arbiter = Mutex::new(Arbiter::default());
        claim(&arbiter, Role::Agent, &[Pad::Gamepad1], |_| {}).unwrap();
        let end = pulse(
            &arbiter,
            Role::Agent,
            Pad::Gamepad1,
            &stick(Pad::Gamepad1, 1.0),
            0,
            |_| {},
        );
        assert_eq!(end, Ok(PulseEnd::Completed { packets: 1 }));
    }

    #[test]
    fn the_operator_takes_over_mid_pulse_and_never_inherits_agent_input() {
        let arbiter = Arc::new(Mutex::new(Arbiter::default()));
        claim(&arbiter, Role::Agent, &[Pad::Gamepad2], |_| {}).unwrap();
        let log: Arc<Mutex<Vec<(Role, Gamepad)>>> = Arc::default();
        let agent = {
            let arbiter = arbiter.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                pulse(
                    &arbiter,
                    Role::Agent,
                    Pad::Gamepad2,
                    &stick(Pad::Gamepad2, 0.8),
                    5_000,
                    |g| log.lock().unwrap().push((Role::Agent, g.clone())),
                )
            })
        };
        std::thread::sleep(Duration::from_millis(100));

        let took_over = Instant::now();
        claim(&arbiter, Role::Operator, &[Pad::Gamepad2], |g| {
            log.lock().unwrap().push((Role::Operator, g.clone()))
        })
        .unwrap();
        let end = agent.join().unwrap().unwrap();
        assert!(
            matches!(
                end,
                PulseEnd::Lost {
                    owner: Some(Role::Operator),
                    ..
                }
            ),
            "{end:?}"
        );
        assert!(
            took_over.elapsed() < Duration::from_secs(1),
            "the pulse did not stop promptly"
        );

        let log = log.lock().unwrap();
        let handover = log
            .iter()
            .position(|(who, _)| *who == Role::Operator)
            .expect("the operator's claim sent a neutral packet");
        assert!(is_neutral(&log[handover].1) && log[handover].1.user == 2);
        assert!(
            log[handover + 1..]
                .iter()
                .all(|(who, _)| *who != Role::Agent),
            "agent input reached the pad after the operator took it"
        );
    }

    #[test]
    fn a_newer_pulse_supersedes_an_older_one() {
        let arbiter = Arc::new(Mutex::new(Arbiter::default()));
        claim(&arbiter, Role::Agent, &[Pad::Gamepad1], |_| {}).unwrap();
        let first = {
            let arbiter = arbiter.clone();
            std::thread::spawn(move || {
                pulse(
                    &arbiter,
                    Role::Agent,
                    Pad::Gamepad1,
                    &stick(Pad::Gamepad1, 0.8),
                    5_000,
                    |_| {},
                )
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        let second = pulse(
            &arbiter,
            Role::Agent,
            Pad::Gamepad1,
            &stick(Pad::Gamepad1, 0.2),
            40,
            |_| {},
        );
        assert_eq!(second, Ok(PulseEnd::Completed { packets: 2 }));
        assert!(matches!(
            first.join().unwrap(),
            Ok(PulseEnd::Superseded { .. })
        ));
    }

    #[test]
    fn status_names_the_owners() {
        let mut arbiter = Arbiter::default();
        arbiter.claim(Role::Operator, &[Pad::Gamepad1]).unwrap();
        assert_eq!(
            arbiter.to_json(),
            json!({"gamepad1": "operator", "gamepad2": null})
        );
    }
}
