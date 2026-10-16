//! Where the phone's route guidance becomes the car's HUD.
//!
//! The car draws a small turn-by-turn strip from `MSG_CMD_NAVI_NEXT_TURN_INFO`
//! — a turn kind, the road the turn leads onto, and how far away it is — and
//! hears that navigation has started or stopped from `MSG_CMD_MODULE_STATUS`.
//! The phone describes the same route over iAP2, in messages shaped for a
//! different question: `RouteGuidanceUpdate` (0x5201) carries the state of the
//! route as a whole and names which of its turns is the current one, and
//! `RouteGuidanceManeuverUpdate` (0x5202) describes one turn at a time, by
//! index, with that turn's own type and the road it leads onto.
//!
//! `Guidance` is where the two are reconciled: `from_iap2` reads one of the
//! phone's messages, `merge` folds it into what is known, and `current` answers
//! which turn the strip is about. The queue it travels on is in `sink`, beside
//! the stream channels and what the phone is playing, because a CarLife session
//! takes it and gives it back on the same terms.
//!
//! Two of the car's six fields are never written, because the phone has nothing
//! behind them. `totalDistance` is the guide's other word for a distance to the
//! next turn — the total against the remaining — and iAP2 describes a turn with
//! one distance only. `time` is the seconds to the turn, and the phone's own
//! countdown is to the destination: a countdown to the wrong place is worse than
//! no countdown.
//!
//! The one distance the car is told is iAP2's `distance_to_maneuver`. The
//! phone's other one, `distance_remaining`, is to the destination — a different
//! place, and not what the car's field means.
//!
//! `turn_kind` is the one table here that knows both vocabularies, and it is
//! deliberately incomplete. A kind is named only where the car's word means the
//! same thing as the phone's; ramps, motorway changes and the arrival types have
//! no counterpart in the car's list at all, and are left unnamed rather than
//! rounded to the nearest arrow.

use std::collections::HashMap;

use iap2_csm::messages::route_guidance::{RouteGuidanceManeuverUpdate, RouteGuidanceUpdate};
use iap2_csm::CsmMessage;

/// One turn of the route, as the phone has described it so far.
///
/// The phone describes each turn once, in a message of its own, and which of
/// them the driver is heading for arrives in a different message — so what is
/// known about a turn and which turn is being shown are two different things.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Maneuver {
    /// iAP2's `EManeuverType`, the 0–53 the table in `turn_kind` names.
    pub kind: Option<u8>,
    /// The road the turn leads onto — iAP2's `after_maneuver_road_name`, which
    /// the phone's own documentation calls the turn-to street.
    pub onto: Option<String>,
}

impl Maneuver {
    /// A later description of the same turn folded in.
    ///
    /// The phone sends the type and the road name in messages of their own, so
    /// what a second one leaves out is what the first one already said.
    fn merge(&mut self, update: Self) {
        if update.kind.is_some() {
            self.kind = update.kind;
        }
        if update.onto.is_some() {
            self.onto = update.onto;
        }
    }
}

/// What the phone has said about the route it is guiding along.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Guidance {
    /// iAP2's `RouteGuidanceState`. See `running` for what the values are.
    state: Option<u8>,
    /// Metres to the next turn — iAP2's `distance_to_maneuver`. Metres is the
    /// unit the car's own field is in, so the number travels unchanged.
    distance_m: Option<u32>,
    /// The turns the phone has described, by the index it gave each one.
    maneuvers: HashMap<u16, Maneuver>,
    /// The indices the guidance is pointing at, in the order it gave them.
    order: Option<Vec<u16>>,
}

/// iAP2's `START_ROUTE`: the step iOS puts at the head of a route's maneuver
/// list before it knows which turn comes first. It is a placeholder rather than
/// an instruction, and it is the only type with no direction at all.
///
/// `START_ROUTE_WITH_U_TURN`, which is a different value, *is* an instruction
/// and is not skipped.
const START_ROUTE: u8 = 11;

impl Guidance {
    /// One of the phone's two route-guidance messages, as far as it goes.
    ///
    /// `None` when the frame is neither: both are the phone's, and a message
    /// this side cannot read says nothing about the route.
    pub fn from_iap2(frame: &[u8]) -> Option<Self> {
        if let Ok(route) = RouteGuidanceUpdate::decode(frame) {
            return Some(Self {
                state: route.state,
                distance_m: route.distance_to_maneuver,
                order: route.current_maneuver_list.as_deref().map(maneuver_order),
                maneuvers: HashMap::new(),
            });
        }
        let turn = RouteGuidanceManeuverUpdate::decode(frame).ok()?;
        // A turn with no index is one nothing can be pointed at, so there is
        // nothing here to keep.
        let index = turn.index?;
        let mut maneuvers = HashMap::new();
        maneuvers.insert(
            index,
            Maneuver {
                kind: turn.maneuver_type,
                onto: turn.after_maneuver_road_name,
            },
        );
        Some(Self {
            maneuvers,
            ..Self::default()
        })
    }

    /// One message from the phone folded into what is known.
    ///
    /// What it left out keeps what it had, because each message describes one
    /// part of the route and says nothing about the rest. A turn the phone
    /// describes twice is that turn described further, not a second turn: it
    /// never retracts one.
    pub fn merge(&mut self, update: Self) {
        let Self {
            state,
            distance_m,
            maneuvers,
            order,
        } = update;
        if state.is_some() {
            self.state = state;
        }
        if distance_m.is_some() {
            self.distance_m = distance_m;
        }
        if order.is_some() {
            self.order = order;
        }
        for (index, turn) in maneuvers {
            self.maneuvers.entry(index).or_default().merge(turn);
        }
    }

    /// Whether the phone says a route is being guided, or nothing at all when
    /// it has not said.
    ///
    /// The guide numbers the state from 0, which is no route, through the five
    /// states a route is in, of which the last is a reroute. Anything above that
    /// is a state this side does not know, and is read as a route being set: a
    /// navigation the phone is describing is not one to report as absent.
    pub fn running(&self) -> Option<bool> {
        Some(self.state? != 0)
    }

    /// Metres to the next turn, once the phone has said.
    pub fn distance_m(&self) -> Option<u32> {
        self.distance_m
    }

    /// The turn the strip is about: the first of the indices the guidance names
    /// that the phone has described, skipping the placeholder iOS puts at the
    /// head of a route.
    ///
    /// The list is the authority rather than the newest description — the phone
    /// describes every turn of the route up front and then says which of them
    /// is current — so a turn is only ever shown when the guidance points at it.
    pub fn current(&self) -> Option<&Maneuver> {
        self.order
            .as_ref()?
            .iter()
            .filter_map(|index| self.maneuvers.get(index))
            .find(|turn| turn.kind != Some(START_ROUTE))
    }
}

/// The two shapes of update, built by hand.
///
/// `from_iap2` is the only way a real one is made, and it is the only way one
/// should be: what these stand in for is a message from a phone that is not
/// here. They live in this module because the fields they set are private to it,
/// and they are for tests — including the ones in `carlife::session`, which has
/// to be able to hand a session a route to draw.
#[cfg(test)]
impl Guidance {
    /// What `RouteGuidanceUpdate` says about the route as a whole.
    pub(crate) fn about_the_route(
        state: Option<u8>,
        distance_m: Option<u32>,
        order: Option<Vec<u16>>,
    ) -> Self {
        Self {
            state,
            distance_m,
            order,
            maneuvers: HashMap::new(),
        }
    }

    /// What one `RouteGuidanceManeuverUpdate` says about one turn.
    pub(crate) fn about_a_turn(index: u16, kind: Option<u8>, onto: Option<&str>) -> Self {
        let mut maneuvers = HashMap::new();
        maneuvers.insert(
            index,
            Maneuver {
                kind,
                onto: onto.map(str::to_string),
            },
        );
        Self {
            maneuvers,
            ..Self::default()
        }
    }
}

/// The indices a `current_maneuver_list` names.
///
/// The field is a byte string holding big-endian 16-bit indices — what is a
/// list in the phone's own model, packed for a transport that carries bytes.
/// A trailing odd byte is not an index and is left out.
fn maneuver_order(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect()
}

/// The car's `TurnKind` values this side has a counterpart for, numbered as the
/// guide's own table of them (guide §13.1, table 13.1-3).
///
/// The car's list runs to fifty-eight values, most of them describing which
/// branch of a fork a turn is on. None of those has an iAP2 counterpart, and
/// they are not here.
const FRONT: i32 = 0x01;
const RIGHT_FRONT: i32 = 0x02;
const RIGHT: i32 = 0x03;
const BACK: i32 = 0x05;
const LEFT: i32 = 0x07;
const LEFT_FRONT: i32 = 0x08;
const RING: i32 = 0x0E;
const IN_FERRY: i32 = 0x0F;
const OUT_FERRY: i32 = 0x10;

/// The car's word for one of the phone's maneuver types, or nothing when it has
/// none.
///
/// The phone's types are `EManeuverType`, 0 to 53, which is Apple's own table
/// for iAP2 and the one CarPlay hands an accessory. The car's are `TurnKind`.
/// The pairs below are the ones where the two words name the same thing:
///
/// ```text
/// 0, 3, 5          no turn, straight on, follow the road   直行
/// 1, 20, 47        left, left at the end of the road, sharp left   左转
/// 2, 21, 48        right, right at the end of the road, sharp right   右转
/// 4, 18, 19, 26    u-turn, and the three that turn back through a roundabout
///                  or when possible   调头
/// 6, 7, 28–46      enter, leave, or leave by one of nineteen exits   环岛
/// 15, 16           onto and off a ferry   进渡口 / 出渡口
/// 49, 50           slight left and right   左前方转弯 / 右前方转弯
/// ```
///
/// The sharp turns lose their sharpness — the car has a plain turn and a turn
/// back, and neither is a sharp one — and the two slight turns are named as the
/// car's "turn to the left front", which is what a slight turn is. What is not
/// here is left out rather than guessed at: an off-ramp, a motorway change, an
/// arrival, and the forks the car describes that the phone does not.
pub fn turn_kind(maneuver_type: u8) -> Option<i32> {
    Some(match maneuver_type {
        0 | 3 | 5 => FRONT,
        1 | 20 | 47 => LEFT,
        2 | 21 | 48 => RIGHT,
        4 | 18 | 19 | 26 => BACK,
        6 | 7 | 28..=46 => RING,
        15 => IN_FERRY,
        16 => OUT_FERRY,
        49 => LEFT_FRONT,
        50 => RIGHT_FRONT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phone's own messages, as a car saw them. `RouteGuidanceUpdate` and
    /// `RouteGuidanceManeuverUpdate` from LIVI's corpus of captured frames, which
    /// its own codec round-trips.
    const GUIDANCE: &str = "4040003c5201000600000014000500010100110003486175707473747261c39f65\
                            00000c0005000000b4e095638000080007000005dc0006000d0102";
    const MANEUVER: &str = "4040001f5202000600010002000500030500080004423237000006000bffa6";

    fn frame(hex: &str) -> Vec<u8> {
        let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        (0..cleaned.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn the_routes_own_message_is_the_state_and_the_turn_list_it_carries() {
        let update = Guidance::from_iap2(&frame(GUIDANCE)).expect("a route guidance update");
        assert_eq!(update.running(), Some(true));
        assert_eq!(update.order, Some(vec![0x0102]));
        // The 1500 m in it is `distance_remaining` — to the destination — and
        // the car's strip is told the distance to the *turn*, which is a
        // different place. A number that does not mean what the field means is
        // worse than a field left empty, so this one is not read at all.
        assert_eq!(update.distance_m(), None);
    }

    /// And the distance the car *is* told is the one to the turn: iAP2's
    /// `distance_to_maneuver`, which LIVI's own JSON calls `remainDistance` —
    /// the car's word for the same number.
    #[test]
    fn the_distance_the_car_is_told_is_the_one_to_the_turn() {
        let update = Guidance::from_iap2(
            &RouteGuidanceUpdate {
                display_component_id: None,
                state: Some(1),
                maneuver_state: None,
                current_road_name: None,
                destination_name: None,
                eta: None,
                time_remaining: None,
                distance_remaining: Some(12_000),
                distance_to_maneuver: Some(350),
                current_maneuver_list: None,
            }
            .encode(),
        )
        .expect("a route guidance update");

        assert_eq!(update.distance_m(), Some(350));
    }

    #[test]
    fn a_turn_is_read_by_the_index_the_phone_gave_it() {
        let update = Guidance::from_iap2(&frame(MANEUVER)).expect("a maneuver update");
        assert_eq!(
            update.maneuvers.get(&2),
            Some(&Maneuver {
                kind: Some(5),
                onto: Some("B27".into()),
            })
        );
    }

    /// The two messages are the two halves of one picture: the turn is described
    /// on its own, and the guidance says which turn is the one being driven to.
    #[test]
    fn the_turn_the_guidance_points_at_is_the_one_it_names() {
        let mut known = Guidance::default();
        known.merge(Guidance::about_a_turn(2, Some(5), Some("B27")));
        assert_eq!(known.current(), None, "nothing points at it yet");

        known.merge(Guidance::about_the_route(Some(1), Some(350), Some(vec![2])));
        assert_eq!(known.current().and_then(|turn| turn.onto.as_deref()), Some("B27"));
        assert_eq!(known.distance_m(), Some(350));
    }

    /// A turn the phone describes twice is the same turn described further, and
    /// the second message carries only what changed.
    #[test]
    fn a_turn_described_twice_keeps_what_the_second_message_left_out() {
        let mut known = Guidance::default();
        known.merge(Guidance::about_a_turn(1, Some(1), Some("Elbchaussee")));
        known.merge(Guidance::about_a_turn(1, None, None));
        known.merge(Guidance::about_a_turn(1, Some(47), None));
        known.merge(Guidance::about_the_route(Some(1), None, Some(vec![1])));

        let turn = known.current().expect("the turn the guidance names");
        assert_eq!(turn.kind, Some(47), "the later type replaced the earlier one");
        assert_eq!(
            turn.onto.as_deref(),
            Some("Elbchaussee"),
            "a message that names no road does not take the road away"
        );
    }

    /// iOS puts its own "start the route" step at the head of the list before it
    /// knows which turn comes first, and it is not a turn to draw.
    #[test]
    fn the_placeholder_at_the_head_of_a_route_is_not_the_turn_shown() {
        let mut known = Guidance::default();
        known.merge(Guidance::about_a_turn(1, Some(11), None));
        known.merge(Guidance::about_a_turn(2, Some(2), Some("B27")));
        known.merge(Guidance::about_the_route(Some(1), None, Some(vec![1, 2])));

        let turn = known.current().expect("the turn behind the placeholder");
        assert_eq!(turn.kind, Some(2));
        assert_eq!(turn.onto.as_deref(), Some("B27"));
    }

    /// The state is what says whether there is a route at all.
    #[test]
    fn the_state_is_what_says_whether_a_route_is_being_guided() {
        assert_eq!(Guidance::default().running(), None, "the phone has not said");
        assert_eq!(Guidance::about_the_route(Some(0), None, None).running(), Some(false));
        for state in 1..=5 {
            assert_eq!(Guidance::about_the_route(Some(state), None, None).running(), Some(true));
        }
    }

    #[test]
    fn a_frame_that_is_neither_message_reads_as_nothing() {
        assert_eq!(Guidance::from_iap2(&[0x40, 0x40, 0x00, 0x06, 0x50, 0x01]), None);
        assert_eq!(Guidance::from_iap2(&[]), None);
    }

    /// The mapping, and the things it deliberately does not map.
    #[test]
    fn the_car_is_given_the_turn_kinds_it_has_a_word_for() {
        // The turns both sides name the same way.
        assert_eq!(turn_kind(1), Some(LEFT));
        assert_eq!(turn_kind(2), Some(RIGHT));
        assert_eq!(turn_kind(3), Some(FRONT));
        assert_eq!(turn_kind(4), Some(BACK));
        assert_eq!(turn_kind(6), Some(RING));
        assert_eq!(turn_kind(15), Some(IN_FERRY));
        assert_eq!(turn_kind(16), Some(OUT_FERRY));
        assert_eq!(turn_kind(49), Some(LEFT_FRONT));
        assert_eq!(turn_kind(50), Some(RIGHT_FRONT));
        // Every roundabout exit is the car's roundabout, whatever its number.
        for exit in 28..=46 {
            assert_eq!(turn_kind(exit), Some(RING), "roundabout exit {exit}");
        }
        // And the whole of the phone's table, so a type that starts or stops
        // being mapped is a failing test rather than a silent change.
        let unmapped: Vec<u8> = (0..=53).filter(|t| turn_kind(*t).is_none()).collect();
        assert_eq!(unmapped, vec![8, 9, 10, 11, 12, 13, 14, 17, 22, 23, 24, 25, 27, 51, 52, 53]);
        assert_eq!(turn_kind(54), None, "54 is not a type the phone has");
    }
}
