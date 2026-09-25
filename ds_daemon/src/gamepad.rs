//! Who may drive each gamepad, and how gamepad input reaches the RC.
//!
//! Each pad has an owner: the operator (`ds_cli`), the agent (`ds_agent`),
//! or nobody. The operator's claim always wins; an agent's claim on a pad
//! the operator holds is refused.
//!
//! Gamepad packets are not acknowledged, so no single datagram may matter:
//! input goes out as a pulse, repeated at 50 Hz for its duration, and every
//! way a pad goes back to rest (the end of a pulse, a claim, a release, a
//! client hanging up mid-pulse) sends a burst of neutral packets. The first
//! neutral packet of a claim or release is sent while the arbiter lock is
//! held, so it reaches the RC before the new owner's first input; nobody
//! inherits a stick someone else left pushed, and re-claiming is how to
//! recover one.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use ds_proto::{GamepadInput, Pad, Role};
use robocol::packets::Gamepad;
use serde_json::{Value, json};

/// Milliseconds between the packets of a pulse or a neutral burst: 50 Hz,
/// as Deck Station sends.
const PULSE_PERIOD_MS: u64 = 20;
/// Pulse length when a `gamepad` request names none.
pub const DEFAULT_PULSE_MS: u64 = 300;
/// The longest pulse one request may ask for.
pub const MAX_PULSE_MS: u64 = 10_000;
/// Input packets in even the shortest pulse, so one lost datagram cannot
/// make a command silently not happen.
pub const MIN_PULSE_PACKETS: u64 = 3;
/// Neutral packets sent after a pulse and on every change of hands, so one
/// lost datagram cannot leave a pad deflected.
pub const TAIL_PACKETS: u64 = 5;

#[derive(Debug, Default, Clone, Copy)]
struct Slot {
    owner: Option<Role>,
    /// Moves on every claim, release and new pulse of this pad. A running
    /// pulse, or a neutral burst, that sees it move stops.
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
                "message": format!(
                    "the operator holds {}; it is free again once the operator releases it or quits ds_cli",
                    pad.name()
                ),
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
    /// A newer pulse by the same owner, or a re-claim, has taken over.
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

    pub fn generation(&self, pad: Pad) -> u64 {
        self.slot(pad).generation
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

/// Rejects stick values outside -1..=1, trigger values outside 0..=1, and
/// anything not a finite number.
pub fn check_input(input: &GamepadInput) -> Result<(), String> {
    let sticks = [
        ("left-stick-x", input.left_stick_x),
        ("left-stick-y", input.left_stick_y),
        ("right-stick-x", input.right_stick_x),
        ("right-stick-y", input.right_stick_y),
    ];
    let triggers = [
        ("left-trigger", input.left_trigger),
        ("right-trigger", input.right_trigger),
    ];
    for (name, value) in sticks {
        if let Some(v) = value
            && !(-1.0..=1.0).contains(&v)
        {
            return Err(format!("--{name} must be between -1 and 1, got {v}"));
        }
    }
    for (name, value) in triggers {
        if let Some(v) = value
            && !(0.0..=1.0).contains(&v)
        {
            return Err(format!("--{name} must be between 0 and 1, got {v}"));
        }
    }
    Ok(())
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

fn sleep_until(deadline: Instant) {
    if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
        std::thread::sleep(wait);
    }
}

/// The deadline of packet `n` of a stream that started at `start`.
fn tick(start: Instant, n: u64) -> Instant {
    start + Duration::from_millis(PULSE_PERIOD_MS * n)
}

/// Finishes a neutral burst whose first packet was already sent: sends each
/// pad's neutral packet again every `PULSE_PERIOD_MS` until `TAIL_PACKETS` have
/// gone out, for as long as nobody has touched the pad since (its
/// generation is unchanged). Takes the arbiter lock per packet, never across
/// a sleep.
fn settle(arbiter: &Mutex<Arbiter>, pads: &[(Pad, u64)], send: &mut impl FnMut(&Gamepad)) {
    let start = Instant::now();
    for n in 1..TAIL_PACKETS {
        sleep_until(tick(start, n));
        let arbiter = arbiter.lock().expect("arbiter mutex poisoned");
        let mut any = false;
        for &(pad, generation) in pads {
            if arbiter.generation(pad) == generation {
                send(&neutral(pad));
                any = true;
            }
        }
        if !any {
            return;
        }
    }
}

/// `Arbiter::claim`, announcing each change and sending each claimed pad's
/// first neutral packet before the lock is released, so both happen in
/// claim order and the neutral reaches the RC ahead of the new owner's
/// input; then the rest of the neutral burst.
pub fn claim(
    arbiter: &Mutex<Arbiter>,
    role: Role,
    pads: &[Pad],
    mut announce: impl FnMut(&OwnerChange),
    mut send: impl FnMut(&Gamepad),
) -> Result<Vec<OwnerChange>, Refusal> {
    let (changes, settling) = {
        let mut arbiter = arbiter.lock().expect("arbiter mutex poisoned");
        let changes = arbiter.claim(role, pads)?;
        for change in &changes {
            announce(change);
        }
        for &pad in pads {
            send(&neutral(pad));
        }
        let settling: Vec<(Pad, u64)> = pads
            .iter()
            .map(|&pad| (pad, arbiter.generation(pad)))
            .collect();
        (changes, settling)
    };
    settle(arbiter, &settling, &mut send);
    Ok(changes)
}

/// `Arbiter::release`, announcing each change and sending a neutral burst
/// for each released pad, the first packet under the lock.
pub fn release(
    arbiter: &Mutex<Arbiter>,
    role: Role,
    pads: &[Pad],
    mut announce: impl FnMut(&OwnerChange),
    mut send: impl FnMut(&Gamepad),
) -> Vec<OwnerChange> {
    let (changes, settling) = {
        let mut arbiter = arbiter.lock().expect("arbiter mutex poisoned");
        let changes = arbiter.release(role, pads);
        for change in &changes {
            announce(change);
            send(&neutral(change.pad));
        }
        let settling: Vec<(Pad, u64)> = changes
            .iter()
            .map(|change| (change.pad, arbiter.generation(change.pad)))
            .collect();
        (changes, settling)
    };
    settle(arbiter, &settling, &mut send);
    changes
}

/// How a pulse ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseEnd {
    /// Ran for its whole duration and returned the pad to neutral.
    Completed { packets: u64 },
    /// A newer pulse by the same owner, or a re-claim, took over; that one
    /// sends its own neutral packets.
    Superseded { packets: u64 },
    /// The pad changed hands mid-pulse. The claim or release that took it
    /// sends the neutral burst.
    Lost { owner: Option<Role>, packets: u64 },
    /// The client that asked for it went away; the pulse stopped early and
    /// returned the pad to neutral.
    Abandoned { packets: u64 },
}

/// Sends `packet` to `pad` every `PULSE_PERIOD_MS` for `duration_ms` (at least
/// `MIN_PULSE_PACKETS` times), then `TAIL_PACKETS` neutral packets.
/// Packets are scheduled against the pulse's start rather than slept for
/// one by one, so a late wake-up does not stretch the pulse. Each packet is
/// checked and sent under the arbiter lock, so none can follow the neutral
/// packet of a claim that took the pad; no lock is held while sleeping.
/// `abandoned` is asked before each input packet: once it says the client is
/// gone, the pulse skips to its neutral tail.
pub fn pulse(
    arbiter: &Mutex<Arbiter>,
    role: Role,
    pad: Pad,
    packet: &Gamepad,
    duration_ms: u64,
    abandoned: impl Fn() -> bool,
    mut send: impl FnMut(&Gamepad),
) -> Result<PulseEnd, Refusal> {
    let generation = arbiter
        .lock()
        .expect("arbiter mutex poisoned")
        .begin_pulse(role, pad)?;
    let ticks = duration_ms.div_ceil(PULSE_PERIOD_MS).max(MIN_PULSE_PACKETS);
    let start = Instant::now();
    let mut packets = 0;
    let mut gave_up = false;
    while packets < ticks {
        sleep_until(tick(start, packets));
        if abandoned() {
            gave_up = true;
            break;
        }
        let arbiter = arbiter.lock().expect("arbiter mutex poisoned");
        match arbiter.pulse_status(role, pad, generation) {
            PulseStatus::Current => send(packet),
            PulseStatus::Superseded => return Ok(PulseEnd::Superseded { packets }),
            PulseStatus::Lost(owner) => return Ok(PulseEnd::Lost { owner, packets }),
        }
        packets += 1;
    }
    let rest = neutral(pad);
    for n in 0..TAIL_PACKETS {
        sleep_until(tick(start, packets + n));
        let arbiter = arbiter.lock().expect("arbiter mutex poisoned");
        if arbiter.pulse_status(role, pad, generation) != PulseStatus::Current {
            break;
        }
        send(&rest);
    }
    Ok(if gave_up {
        PulseEnd::Abandoned { packets }
    } else {
        PulseEnd::Completed { packets }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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

    fn nobody_announces(_: &OwnerChange) {}

    fn never_abandoned() -> bool {
        false
    }

    /// Claims without recording anything, for tests that only need the pad.
    fn take(arbiter: &Mutex<Arbiter>, role: Role, pads: &[Pad]) {
        claim(arbiter, role, pads, nobody_announces, |_| {}).unwrap();
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
    fn input_out_of_range_is_refused() {
        let with = |f: fn(&mut GamepadInput)| {
            let mut input = GamepadInput::default();
            f(&mut input);
            check_input(&input)
        };
        assert!(with(|i| i.left_stick_y = Some(-1.0)).is_ok());
        assert!(with(|i| i.left_stick_y = Some(1.5)).is_err());
        assert!(with(|i| i.right_stick_x = Some(f32::NAN)).is_err());
        assert!(with(|i| i.left_trigger = Some(-0.1)).is_err());
        assert!(with(|i| i.right_trigger = Some(1.0)).is_ok());
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
    fn every_claim_sends_a_neutral_burst_even_for_a_pad_already_held() {
        let arbiter = Mutex::new(Arbiter::default());
        let mut sent = Vec::new();
        claim(
            &arbiter,
            Role::Agent,
            &[Pad::Gamepad2],
            nobody_announces,
            |g| sent.push(g.clone()),
        )
        .unwrap();
        let again = claim(
            &arbiter,
            Role::Agent,
            &[Pad::Gamepad2],
            nobody_announces,
            |g| sent.push(g.clone()),
        )
        .unwrap();
        assert!(again.is_empty(), "re-claiming changes no owner");
        assert_eq!(sent.len(), 2 * TAIL_PACKETS as usize);
        assert!(sent.iter().all(|g| g.user == 2 && is_neutral(g)));
    }

    #[test]
    fn changes_are_announced_under_the_claim() {
        let arbiter = Mutex::new(Arbiter::default());
        let mut announced = Vec::new();
        claim(
            &arbiter,
            Role::Agent,
            &Pad::ALL,
            |c| announced.push(*c),
            |_| {},
        )
        .unwrap();
        claim(
            &arbiter,
            Role::Operator,
            &[Pad::Gamepad2],
            |c| announced.push(*c),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            announced,
            vec![
                OwnerChange {
                    pad: Pad::Gamepad1,
                    owner: Some(Role::Agent),
                    previous: None
                },
                OwnerChange {
                    pad: Pad::Gamepad2,
                    owner: Some(Role::Agent),
                    previous: None
                },
                OwnerChange {
                    pad: Pad::Gamepad2,
                    owner: Some(Role::Operator),
                    previous: Some(Role::Agent),
                },
            ]
        );
    }

    #[test]
    fn release_returns_only_pads_you_hold() {
        let arbiter = Mutex::new(Arbiter::default());
        take(&arbiter, Role::Agent, &[Pad::Gamepad1]);
        let mut sent = Vec::new();
        let changes = release(&arbiter, Role::Agent, &Pad::ALL, nobody_announces, |g| {
            sent.push(g.clone())
        });
        assert_eq!(
            changes,
            vec![OwnerChange {
                pad: Pad::Gamepad1,
                owner: None,
                previous: Some(Role::Agent),
            }]
        );
        assert_eq!(sent.len(), TAIL_PACKETS as usize);
        assert!(sent.iter().all(|g| g.user == 1 && is_neutral(g)));
        let nothing = release(
            &arbiter,
            Role::Operator,
            &Pad::ALL,
            nobody_announces,
            |_| panic!("nothing held"),
        );
        assert!(nothing.is_empty());
    }

    #[test]
    fn a_new_owners_input_cuts_the_neutral_burst_short() {
        // The operator claims, then starts driving straight away: its input
        // must not be interleaved with the rest of the claim's burst.
        let arbiter = Arc::new(Mutex::new(Arbiter::default()));
        let log: Arc<Mutex<Vec<(&str, Gamepad)>>> = Arc::default();
        let claiming = {
            let arbiter = arbiter.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                claim(
                    &arbiter,
                    Role::Operator,
                    &[Pad::Gamepad1],
                    nobody_announces,
                    |g| log.lock().unwrap().push(("claim", g.clone())),
                )
                .unwrap()
            })
        };
        std::thread::sleep(Duration::from_millis(30));
        pulse(
            &arbiter,
            Role::Operator,
            Pad::Gamepad1,
            &stick(Pad::Gamepad1, 0.5),
            100,
            never_abandoned,
            |g| log.lock().unwrap().push(("pulse", g.clone())),
        )
        .unwrap();
        claiming.join().unwrap();

        let log = log.lock().unwrap();
        let first_input = log
            .iter()
            .position(|(who, _)| *who == "pulse")
            .expect("the pulse sent input");
        assert!(
            log[first_input..].iter().all(|(who, _)| *who == "pulse"),
            "a claim neutral arrived after the new owner's input: {log:?}"
        );
    }

    #[test]
    fn a_pulse_needs_the_pad() {
        let arbiter = Mutex::new(Arbiter::default());
        let packet = stick(Pad::Gamepad1, 0.5);
        let unclaimed = pulse(
            &arbiter,
            Role::Agent,
            Pad::Gamepad1,
            &packet,
            20,
            never_abandoned,
            |_| panic!("sent unclaimed"),
        );
        assert_eq!(unclaimed, Err(Refusal::NotClaimed(Pad::Gamepad1)));
        take(&arbiter, Role::Operator, &[Pad::Gamepad1]);
        let taken = pulse(
            &arbiter,
            Role::Agent,
            Pad::Gamepad1,
            &packet,
            20,
            never_abandoned,
            |_| panic!("sent preempted"),
        );
        assert_eq!(taken, Err(Refusal::Preempted(Pad::Gamepad1)));
    }

    #[test]
    fn a_pulse_streams_its_input_then_a_neutral_tail() {
        let arbiter = Mutex::new(Arbiter::default());
        take(&arbiter, Role::Agent, &[Pad::Gamepad2]);
        let mut sent = Vec::new();
        let packet = stick(Pad::Gamepad2, -0.5);
        let end = pulse(
            &arbiter,
            Role::Agent,
            Pad::Gamepad2,
            &packet,
            100,
            never_abandoned,
            |g| sent.push(g.clone()),
        );
        assert_eq!(end, Ok(PulseEnd::Completed { packets: 5 }));
        assert_eq!(sent.len(), 5 + TAIL_PACKETS as usize);
        assert!(sent[..5].iter().all(|g| *g == packet));
        assert!(sent[5..].iter().all(|g| g.user == 2 && is_neutral(g)));
    }

    #[test]
    fn a_pulse_keeps_time() {
        // Scheduled against its start, a 200 ms pulse sends 10 packets about
        // 20 ms apart, however late each individual wake-up is.
        let arbiter = Mutex::new(Arbiter::default());
        take(&arbiter, Role::Agent, &[Pad::Gamepad1]);
        let mut input_times = Vec::new();
        let packet = stick(Pad::Gamepad1, 0.5);
        pulse(
            &arbiter,
            Role::Agent,
            Pad::Gamepad1,
            &packet,
            200,
            never_abandoned,
            |g| {
                if !is_neutral(g) {
                    input_times.push(Instant::now());
                }
            },
        )
        .unwrap();
        assert_eq!(input_times.len(), 10);
        let span = input_times[9] - input_times[0];
        assert!(
            (Duration::from_millis(150)..=Duration::from_millis(260)).contains(&span),
            "10 packets spanned {span:?}, expected about 180 ms"
        );
    }

    #[test]
    fn even_a_zero_length_pulse_sends_several_packets() {
        let arbiter = Mutex::new(Arbiter::default());
        take(&arbiter, Role::Agent, &[Pad::Gamepad1]);
        let end = pulse(
            &arbiter,
            Role::Agent,
            Pad::Gamepad1,
            &stick(Pad::Gamepad1, 1.0),
            0,
            never_abandoned,
            |_| {},
        );
        assert_eq!(
            end,
            Ok(PulseEnd::Completed {
                packets: MIN_PULSE_PACKETS
            })
        );
    }

    #[test]
    fn an_abandoned_pulse_stops_and_returns_to_neutral() {
        let arbiter = Mutex::new(Arbiter::default());
        take(&arbiter, Role::Agent, &[Pad::Gamepad2]);
        let gone = AtomicBool::new(false);
        let started = Instant::now();
        let mut sent = Vec::new();
        let end = pulse(
            &arbiter,
            Role::Agent,
            Pad::Gamepad2,
            &stick(Pad::Gamepad2, 0.75),
            5_000,
            || {
                if started.elapsed() > Duration::from_millis(60) {
                    gone.store(true, Ordering::Relaxed);
                }
                gone.load(Ordering::Relaxed)
            },
            |g| sent.push(g.clone()),
        )
        .unwrap();
        assert!(
            matches!(end, PulseEnd::Abandoned { packets } if packets < 10),
            "{end:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let tail = &sent[sent.len() - TAIL_PACKETS as usize..];
        assert!(tail.iter().all(|g| g.user == 2 && is_neutral(g)));
    }

    #[test]
    fn the_operator_takes_over_mid_pulse_and_never_inherits_agent_input() {
        let arbiter = Arc::new(Mutex::new(Arbiter::default()));
        take(&arbiter, Role::Agent, &[Pad::Gamepad2]);
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
                    never_abandoned,
                    |g| log.lock().unwrap().push((Role::Agent, g.clone())),
                )
            })
        };
        std::thread::sleep(Duration::from_millis(100));

        let took_over = Instant::now();
        claim(
            &arbiter,
            Role::Operator,
            &[Pad::Gamepad2],
            nobody_announces,
            |g| log.lock().unwrap().push((Role::Operator, g.clone())),
        )
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
        assert!(
            log[handover..]
                .iter()
                .all(|(who, g)| *who == Role::Operator && is_neutral(g) && g.user == 2),
            "agent input reached the pad after the operator took it"
        );
        assert_eq!(log.len() - handover, TAIL_PACKETS as usize);
    }

    #[test]
    fn a_newer_pulse_supersedes_an_older_one() {
        let arbiter = Arc::new(Mutex::new(Arbiter::default()));
        take(&arbiter, Role::Agent, &[Pad::Gamepad1]);
        let first = {
            let arbiter = arbiter.clone();
            std::thread::spawn(move || {
                pulse(
                    &arbiter,
                    Role::Agent,
                    Pad::Gamepad1,
                    &stick(Pad::Gamepad1, 0.8),
                    5_000,
                    never_abandoned,
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
            80,
            never_abandoned,
            |_| {},
        );
        assert_eq!(second, Ok(PulseEnd::Completed { packets: 4 }));
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
