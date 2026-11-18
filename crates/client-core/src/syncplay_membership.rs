//! SyncPlay membership: the join/leave request lifecycle as an explicit, pure machine.
//!
//! The group's *playback* state already has a pure machine in [`crate::syncplay`]
//! (`apply_server_event`). What stayed implicit in the app was **membership**: a pending join
//! (`Option<(request_id, group_id)>`), a pending leave (`Option<request_id>`), and four event arms
//! that each re-checked the request id by hand. A result for an abandoned request was dropped by a
//! guard in every arm, and "may this client send a group command" was recomputed from two loose
//! fields.
//!
//! This machine owns that lifecycle. Every transition that finishes an action declares the effects
//! the caller runs, and a result whose request id does not match the pending request changes
//! nothing. Pure: no I/O, no clock, no threads.

/// Where the membership lifecycle is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MembershipState {
    /// Not in a group and no request in flight.
    #[default]
    Out,
    /// A join is in flight. `previous` is the group that was joined before it, if any, so a failed
    /// join can put the client back where it was instead of dropping it to `Out`.
    Joining {
        request_id: u64,
        group_id: String,
        previous: Option<String>,
    },
    /// In the group.
    In { group_id: String },
    /// A leave is in flight. The client is still a member until the server confirms.
    Leaving { request_id: u64, group_id: String },
}

impl MembershipState {
    /// The group this client is in or is trying to be in.
    pub fn group_id(&self) -> Option<&str> {
        match self {
            Self::Out => None,
            Self::Joining { group_id, .. }
            | Self::In { group_id }
            | Self::Leaving { group_id, .. } => Some(group_id),
        }
    }

    /// Whether the client is a member (the server still considers it joined).
    pub fn is_member(&self) -> bool {
        matches!(self, Self::In { .. } | Self::Leaving { .. })
    }
}

/// Inputs to the membership lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MembershipEvent {
    /// The viewer asked to join; the caller has already queued the request under `request_id`.
    JoinRequested { request_id: u64, group_id: String },
    /// The worker confirmed the join.
    JoinConfirmed { request_id: u64, group_id: String },
    /// The worker refused the join.
    JoinFailed { request_id: u64 },
    /// The viewer asked to leave; the caller has already queued the request under `request_id`.
    LeaveRequested { request_id: u64 },
    /// The worker confirmed the leave.
    LeaveConfirmed { request_id: u64 },
    /// The worker refused the leave.
    LeaveFailed { request_id: u64 },
    /// The session socket reported that this client is now in `group_id`.
    GroupEnteredFromServer { group_id: String },
    /// The caller learned about a membership through another path (a group playback command or the
    /// group screen) and tells the machine about it. No effects: the caller's state is already
    /// correct, the machine only needs to agree so a later leave can be requested.
    AdoptCurrentGroup { group_id: String },
    /// The session socket reported that this client left the group. `group_id` is the group the
    /// server named, when it named one; it must match the current group to take effect.
    GroupLeftFromServer { group_id: Option<String> },
    /// The session ended (sign-out, reconnect); drop the lifecycle without UI feedback.
    Reset,
}

/// Work the caller must do after a transition. Declared, not executed here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MembershipEffect {
    /// The app's group playback state becomes `Joined` for this group.
    EnterGroup { group_id: String },
    /// The app's group playback state becomes `NotJoined`.
    LeaveGroup,
    /// Advance the queue generation and drop the queued playback.
    InvalidateQueue,
    /// Clear the pending "latest action" request id.
    ClearLatestAction,
    /// A join/leave action succeeded (UI feedback).
    ActionSucceeded,
    /// A join/leave action failed (UI feedback).
    ActionFailed,
}

/// One transition: the next state and the effects that make it true in the outside world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipTransition {
    pub state: MembershipState,
    pub effects: Vec<MembershipEffect>,
}

/// Owns the SyncPlay membership lifecycle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncPlayMembershipMachine {
    state: MembershipState,
}

impl SyncPlayMembershipMachine {
    pub fn new() -> Self {
        Self {
            state: MembershipState::Out,
        }
    }

    pub fn state(&self) -> &MembershipState {
        &self.state
    }

    /// The group this client is in or joining, if any.
    pub fn group_id(&self) -> Option<&str> {
        self.state.group_id()
    }

    /// Whether a join request is in flight.
    pub fn is_join_pending(&self) -> bool {
        matches!(self.state, MembershipState::Joining { .. })
    }

    /// Whether a leave request is in flight.
    pub fn is_leave_pending(&self) -> bool {
        matches!(self.state, MembershipState::Leaving { .. })
    }

    /// Whether the client may send a group playback command: a member, and not leaving.
    pub fn can_send(&self) -> bool {
        matches!(self.state, MembershipState::In { .. })
    }

    /// The request id of the pending join or leave, if any.
    pub fn pending_request_id(&self) -> Option<u64> {
        match &self.state {
            MembershipState::Joining { request_id, .. }
            | MembershipState::Leaving { request_id, .. } => Some(*request_id),
            MembershipState::Out | MembershipState::In { .. } => None,
        }
    }

    /// Advance by one event. A result whose request id does not match the pending request changes
    /// nothing: that single check replaces the per-arm guards the app used to repeat.
    pub fn reduce(&mut self, event: MembershipEvent) -> MembershipTransition {
        use MembershipEffect as F;
        use MembershipEvent as E;
        use MembershipState as S;

        let (state, effects) = match (&self.state, event) {
            (_, E::Reset) => (S::Out, Vec::new()),

            (
                _,
                E::JoinRequested {
                    request_id,
                    group_id,
                },
            ) => {
                let previous = self.state.group_id().map(str::to_owned);
                (
                    S::Joining {
                        request_id,
                        group_id,
                        previous,
                    },
                    Vec::new(),
                )
            }

            (
                S::Joining {
                    request_id: pending,
                    previous,
                    ..
                },
                E::JoinConfirmed {
                    request_id,
                    group_id,
                },
            ) if *pending == request_id => {
                let mut effects = Vec::new();
                if previous.as_deref() != Some(group_id.as_str()) {
                    effects.push(F::InvalidateQueue);
                }
                effects.push(F::EnterGroup {
                    group_id: group_id.clone(),
                });
                effects.push(F::ClearLatestAction);
                effects.push(F::ActionSucceeded);
                (S::In { group_id }, effects)
            }

            (
                S::Joining {
                    request_id: pending,
                    previous,
                    ..
                },
                E::JoinFailed { request_id },
            ) if *pending == request_id => {
                let restored = match previous {
                    Some(group_id) => S::In {
                        group_id: group_id.clone(),
                    },
                    None => S::Out,
                };
                (restored, vec![F::ActionFailed])
            }

            (S::In { group_id }, E::LeaveRequested { request_id }) => (
                S::Leaving {
                    request_id,
                    group_id: group_id.clone(),
                },
                Vec::new(),
            ),

            (
                S::Leaving {
                    request_id: pending,
                    ..
                },
                E::LeaveConfirmed { request_id },
            ) if *pending == request_id => (
                S::Out,
                vec![
                    F::InvalidateQueue,
                    F::ClearLatestAction,
                    F::LeaveGroup,
                    F::ActionSucceeded,
                ],
            ),

            (
                S::Leaving {
                    request_id: pending,
                    group_id,
                },
                E::LeaveFailed { request_id },
            ) if *pending == request_id => (
                S::In {
                    group_id: group_id.clone(),
                },
                vec![F::ActionFailed],
            ),

            (_, E::GroupEnteredFromServer { group_id }) => {
                let changed = self.state.group_id() != Some(group_id.as_str());
                let mut effects = Vec::new();
                if changed {
                    effects.push(F::InvalidateQueue);
                    effects.push(F::EnterGroup {
                        group_id: group_id.clone(),
                    });
                }
                effects.push(F::ActionSucceeded);
                (S::In { group_id }, effects)
            }

            (_, E::AdoptCurrentGroup { group_id }) => (S::In { group_id }, Vec::new()),

            (current, E::GroupLeftFromServer { group_id }) => {
                let matches = current.is_member()
                    && match group_id.as_deref() {
                        None => true,
                        Some(group) => current.group_id() == Some(group),
                    };
                if matches {
                    (
                        S::Out,
                        vec![F::InvalidateQueue, F::LeaveGroup, F::ClearLatestAction],
                    )
                } else {
                    (current.clone(), Vec::new())
                }
            }

            // A stale result or an event that does not apply here: change nothing.
            (current, _) => (current.clone(), Vec::new()),
        };

        self.state = state;
        MembershipTransition {
            state: self.state.clone(),
            effects,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MembershipEffect, MembershipEvent, MembershipState, SyncPlayMembershipMachine};

    fn machine_in(group_id: &str) -> SyncPlayMembershipMachine {
        let mut machine = SyncPlayMembershipMachine::new();
        let _ = machine.reduce(MembershipEvent::GroupEnteredFromServer {
            group_id: group_id.to_owned(),
        });
        assert!(machine.can_send());
        machine
    }

    #[test]
    fn a_join_request_moves_to_joining_without_effects() {
        let mut machine = SyncPlayMembershipMachine::new();
        let transition = machine.reduce(MembershipEvent::JoinRequested {
            request_id: 7,
            group_id: "group-a".into(),
        });
        assert_eq!(
            transition.state,
            MembershipState::Joining {
                request_id: 7,
                group_id: "group-a".into(),
                previous: None,
            }
        );
        assert!(transition.effects.is_empty());
        assert!(machine.is_join_pending());
        assert!(!machine.can_send());
    }

    #[test]
    fn a_confirmed_join_enters_the_group_and_invalidates_the_queue() {
        let mut machine = SyncPlayMembershipMachine::new();
        let _ = machine.reduce(MembershipEvent::JoinRequested {
            request_id: 7,
            group_id: "group-a".into(),
        });
        let transition = machine.reduce(MembershipEvent::JoinConfirmed {
            request_id: 7,
            group_id: "group-a".into(),
        });
        assert_eq!(
            transition.state,
            MembershipState::In {
                group_id: "group-a".into()
            }
        );
        assert_eq!(
            transition.effects,
            vec![
                MembershipEffect::InvalidateQueue,
                MembershipEffect::EnterGroup {
                    group_id: "group-a".into()
                },
                MembershipEffect::ClearLatestAction,
                MembershipEffect::ActionSucceeded,
            ]
        );
        assert!(machine.can_send());
    }

    #[test]
    fn rejoining_the_same_group_keeps_the_queue() {
        let mut machine = machine_in("group-a");
        let _ = machine.reduce(MembershipEvent::JoinRequested {
            request_id: 8,
            group_id: "group-a".into(),
        });
        let transition = machine.reduce(MembershipEvent::JoinConfirmed {
            request_id: 8,
            group_id: "group-a".into(),
        });
        assert!(
            !transition
                .effects
                .contains(&MembershipEffect::InvalidateQueue),
            "the queue for this group is still current"
        );
        assert!(matches!(transition.state, MembershipState::In { .. }));
    }

    #[test]
    fn a_stale_join_confirmation_is_ignored() {
        let mut machine = SyncPlayMembershipMachine::new();
        let _ = machine.reduce(MembershipEvent::JoinRequested {
            request_id: 7,
            group_id: "group-a".into(),
        });
        let transition = machine.reduce(MembershipEvent::JoinConfirmed {
            request_id: 6,
            group_id: "group-a".into(),
        });
        assert_eq!(
            transition.state,
            MembershipState::Joining {
                request_id: 7,
                group_id: "group-a".into(),
                previous: None,
            }
        );
        assert!(transition.effects.is_empty());
        assert!(machine.is_join_pending());
    }

    #[test]
    fn a_failed_join_from_out_goes_back_to_out() {
        let mut machine = SyncPlayMembershipMachine::new();
        let _ = machine.reduce(MembershipEvent::JoinRequested {
            request_id: 7,
            group_id: "group-a".into(),
        });
        let transition = machine.reduce(MembershipEvent::JoinFailed { request_id: 7 });
        assert_eq!(transition.state, MembershipState::Out);
        assert_eq!(transition.effects, vec![MembershipEffect::ActionFailed]);
        assert!(!machine.is_join_pending());
    }

    #[test]
    fn a_failed_join_while_switching_groups_restores_the_previous_group() {
        let mut machine = machine_in("group-a");
        let _ = machine.reduce(MembershipEvent::JoinRequested {
            request_id: 9,
            group_id: "group-b".into(),
        });
        let transition = machine.reduce(MembershipEvent::JoinFailed { request_id: 9 });
        assert_eq!(
            transition.state,
            MembershipState::In {
                group_id: "group-a".into()
            }
        );
        assert!(machine.can_send());
    }

    #[test]
    fn a_leave_keeps_membership_until_the_server_confirms() {
        let mut machine = machine_in("group-a");
        let _ = machine.reduce(MembershipEvent::LeaveRequested { request_id: 3 });
        assert!(machine.is_leave_pending());
        assert!(
            machine.state().is_member(),
            "still a member until confirmed"
        );
        assert!(!machine.can_send(), "no commands while leaving");

        let transition = machine.reduce(MembershipEvent::LeaveConfirmed { request_id: 3 });
        assert_eq!(transition.state, MembershipState::Out);
        assert_eq!(
            transition.effects,
            vec![
                MembershipEffect::InvalidateQueue,
                MembershipEffect::ClearLatestAction,
                MembershipEffect::LeaveGroup,
                MembershipEffect::ActionSucceeded,
            ]
        );
        assert!(!machine.state().is_member());
    }

    #[test]
    fn a_failed_leave_restores_the_group() {
        let mut machine = machine_in("group-a");
        let _ = machine.reduce(MembershipEvent::LeaveRequested { request_id: 3 });
        let transition = machine.reduce(MembershipEvent::LeaveFailed { request_id: 3 });
        assert_eq!(
            transition.state,
            MembershipState::In {
                group_id: "group-a".into()
            }
        );
        assert_eq!(transition.effects, vec![MembershipEffect::ActionFailed]);
        assert!(machine.can_send());
    }

    #[test]
    fn stale_leave_results_are_ignored() {
        let mut machine = machine_in("group-a");
        let _ = machine.reduce(MembershipEvent::LeaveRequested { request_id: 3 });
        for event in [
            MembershipEvent::LeaveConfirmed { request_id: 2 },
            MembershipEvent::LeaveFailed { request_id: 4 },
        ] {
            let transition = machine.reduce(event);
            assert!(transition.effects.is_empty());
            assert!(machine.is_leave_pending());
        }
    }

    #[test]
    fn the_socket_reporting_the_same_group_does_not_invalidate_the_queue() {
        let mut machine = machine_in("group-a");
        let transition = machine.reduce(MembershipEvent::GroupEnteredFromServer {
            group_id: "group-a".into(),
        });
        assert!(
            !transition
                .effects
                .contains(&MembershipEffect::InvalidateQueue)
        );
        assert_eq!(transition.effects, vec![MembershipEffect::ActionSucceeded]);
    }

    #[test]
    fn the_socket_reporting_a_new_group_invalidates_the_queue() {
        let mut machine = machine_in("group-a");
        let transition = machine.reduce(MembershipEvent::GroupEnteredFromServer {
            group_id: "group-b".into(),
        });
        assert!(
            transition
                .effects
                .contains(&MembershipEffect::InvalidateQueue)
        );
        assert!(transition.effects.contains(&MembershipEffect::EnterGroup {
            group_id: "group-b".into()
        }));
    }

    #[test]
    fn a_leave_report_matches_the_named_group_or_any_group() {
        let mut named = machine_in("group-a");
        let wrong = named.reduce(MembershipEvent::GroupLeftFromServer {
            group_id: Some("group-b".into()),
        });
        assert!(wrong.effects.is_empty(), "another group's leave is ignored");
        assert!(named.can_send());

        let right = named.reduce(MembershipEvent::GroupLeftFromServer {
            group_id: Some("group-a".into()),
        });
        assert_eq!(right.state, MembershipState::Out);
        assert!(right.effects.contains(&MembershipEffect::LeaveGroup));

        let mut any = machine_in("group-a");
        let unnamed = any.reduce(MembershipEvent::GroupLeftFromServer { group_id: None });
        assert_eq!(unnamed.state, MembershipState::Out);
    }

    #[test]
    fn a_leave_report_while_out_changes_nothing() {
        let mut machine = SyncPlayMembershipMachine::new();
        let transition = machine.reduce(MembershipEvent::GroupLeftFromServer { group_id: None });
        assert!(transition.effects.is_empty());
        assert_eq!(transition.state, MembershipState::Out);
    }

    #[test]
    fn reset_drops_the_lifecycle_without_ui_feedback() {
        let mut machine = machine_in("group-a");
        let _ = machine.reduce(MembershipEvent::LeaveRequested { request_id: 3 });
        let transition = machine.reduce(MembershipEvent::Reset);
        assert_eq!(transition.state, MembershipState::Out);
        assert!(transition.effects.is_empty());
        assert!(!machine.is_leave_pending());
        assert!(!machine.can_send());
    }

    #[test]
    fn a_leave_request_while_out_is_refused() {
        let mut machine = SyncPlayMembershipMachine::new();
        let transition = machine.reduce(MembershipEvent::LeaveRequested { request_id: 3 });
        assert!(transition.effects.is_empty());
        assert_eq!(transition.state, MembershipState::Out);
        assert!(!machine.is_leave_pending());
    }

    #[test]
    fn a_second_join_request_replaces_the_pending_one() {
        let mut machine = SyncPlayMembershipMachine::new();
        let _ = machine.reduce(MembershipEvent::JoinRequested {
            request_id: 1,
            group_id: "group-a".into(),
        });
        let _ = machine.reduce(MembershipEvent::JoinRequested {
            request_id: 2,
            group_id: "group-b".into(),
        });
        let transition = machine.reduce(MembershipEvent::JoinConfirmed {
            request_id: 2,
            group_id: "group-b".into(),
        });
        assert!(matches!(transition.state, MembershipState::In { .. }));
    }
}
