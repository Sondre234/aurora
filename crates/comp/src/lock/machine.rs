//! The session-lock state machine, pure (no Smithay, no Wayland) so its rules are unit tested.
//!
//! Rules:
//! - A lock request starts `Locking`; the compositor confirms only once every output has a
//!   lock surface that committed a buffer, and never with zero outputs.
//! - The client dying before confirmation cancels the attempt: the session is unlocked again,
//!   unless it was a takeover of a dead lock, which stays locked.
//! - The client dying while `Locked` leaves the session `Locked` with no owner: outputs show
//!   black. Another client may then take over; until it confirms the session stays engaged.
//! - Unlocking is only possible through the owner's unlock request. A dead owner cannot
//!   send one, and nothing else (a config, an action, a timeout) unlocks.

/// Identifies one lock request (one `ext_session_lock_v1` object).
pub type LockId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Unlocked,
    Locking,
    Locked,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Unlocked => "unlocked",
            Phase::Locking => "locking",
            Phase::Locked => "locked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Unlocked,
    /// `takeover`: the session was already locked by a client that died.
    Locking {
        owner: LockId,
        takeover: bool,
    },
    /// `owner` is `None` once the locking client is gone.
    Locked {
        owner: Option<LockId>,
    },
}

/// What a lock request got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accepted(LockId),
    Denied,
}

/// What losing the lock client changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lost {
    /// Not the owner, or nothing to do.
    Nothing,
    /// An unconfirmed attempt was abandoned; the session is unlocked again.
    Cancelled,
    /// The session stays locked with nobody behind it.
    Orphaned,
}

#[derive(Debug)]
struct Surface {
    output: String,
    committed: bool,
}

#[derive(Debug)]
pub struct LockMachine {
    state: State,
    next_id: LockId,
    outputs: Vec<String>,
    surfaces: Vec<Surface>,
}

impl Default for LockMachine {
    fn default() -> Self {
        Self {
            state: State::Unlocked,
            next_id: 1,
            outputs: Vec::new(),
            surfaces: Vec::new(),
        }
    }
}

impl LockMachine {
    pub fn phase(&self) -> Phase {
        match self.state {
            State::Unlocked => Phase::Unlocked,
            State::Locking { .. } => Phase::Locking,
            State::Locked { .. } => Phase::Locked,
        }
    }

    /// The id of the client behind the lock, if it is still there.
    pub fn owner(&self) -> Option<LockId> {
        match self.state {
            State::Unlocked | State::Locked { owner: None } => None,
            State::Locking { owner, .. } => Some(owner),
            State::Locked { owner } => owner,
        }
    }

    pub fn surface_count(&self) -> usize {
        self.surfaces.len()
    }

    pub fn has_surface(&self, output: &str) -> bool {
        self.surfaces.iter().any(|s| s.output == output)
    }

    /// A client asked to lock. `outputs` is what exists right now.
    pub fn request(&mut self, outputs: &[String]) -> Verdict {
        let takeover = match self.state {
            State::Unlocked => false,
            State::Locked { owner: None } => true,
            State::Locking { .. } | State::Locked { owner: Some(_) } => return Verdict::Denied,
        };
        let id = self.next_id;
        self.next_id += 1;
        self.state = State::Locking {
            owner: id,
            takeover,
        };
        self.outputs = outputs.to_vec();
        self.surfaces.clear();
        Verdict::Accepted(id)
    }

    /// The set of outputs changed: surfaces of vanished outputs go away.
    pub fn set_outputs(&mut self, outputs: &[String]) {
        self.outputs = outputs.to_vec();
        let keep = &self.outputs;
        self.surfaces.retain(|s| keep.contains(&s.output));
    }

    /// `owner` made a lock surface for `output`. False when it is not the current owner,
    /// the output is unknown, or the output already has one.
    pub fn surface_added(&mut self, owner: LockId, output: &str) -> bool {
        if self.owner() != Some(owner)
            || !self.outputs.iter().any(|o| o == output)
            || self.has_surface(output)
        {
            return false;
        }
        self.surfaces.push(Surface {
            output: output.to_owned(),
            committed: false,
        });
        true
    }

    /// The surface of `output` committed a buffer.
    pub fn surface_committed(&mut self, output: &str) {
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.output == output) {
            s.committed = true;
        }
    }

    pub fn surface_removed(&mut self, output: &str) {
        self.surfaces.retain(|s| s.output != output);
    }

    /// Every output has a committed lock surface.
    pub fn ready_to_confirm(&self) -> bool {
        matches!(self.state, State::Locking { .. })
            && !self.outputs.is_empty()
            && self
                .outputs
                .iter()
                .all(|o| self.surfaces.iter().any(|s| &s.output == o && s.committed))
    }

    /// The compositor told the client `locked`. False unless it was ready.
    pub fn confirmed(&mut self) -> bool {
        if !self.ready_to_confirm() {
            return false;
        }
        if let State::Locking { owner, .. } = self.state {
            self.state = State::Locked { owner: Some(owner) };
        }
        true
    }

    /// The owner's `unlock_and_destroy`. False (still locked) unless the session is locked
    /// with a living owner.
    pub fn unlock(&mut self) -> bool {
        if matches!(self.state, State::Locked { owner: Some(_) }) {
            self.state = State::Unlocked;
            self.surfaces.clear();
            true
        } else {
            false
        }
    }

    /// The lock client `id` is gone.
    pub fn client_gone(&mut self, id: LockId) -> Lost {
        match self.state {
            State::Locking {
                owner,
                takeover: false,
            } if owner == id => {
                self.state = State::Unlocked;
                self.surfaces.clear();
                Lost::Cancelled
            }
            State::Locking {
                owner,
                takeover: true,
            } if owner == id => {
                self.state = State::Locked { owner: None };
                self.surfaces.clear();
                Lost::Orphaned
            }
            State::Locked { owner: Some(owner) } if owner == id => {
                self.state = State::Locked { owner: None };
                self.surfaces.clear();
                Lost::Orphaned
            }
            _ => Lost::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn locked(names: &[&str]) -> (LockMachine, LockId) {
        let mut m = LockMachine::default();
        let Verdict::Accepted(id) = m.request(&outs(names)) else {
            panic!("denied");
        };
        for n in names {
            assert!(m.surface_added(id, n));
            m.surface_committed(n);
        }
        assert!(m.confirmed());
        (m, id)
    }

    #[test]
    fn starts_unlocked() {
        let m = LockMachine::default();
        assert_eq!(m.phase(), Phase::Unlocked);
        assert_eq!(m.owner(), None);
    }

    #[test]
    fn request_engages_at_once() {
        let mut m = LockMachine::default();
        assert!(matches!(m.request(&outs(&["A"])), Verdict::Accepted(_)));
        assert_eq!(m.phase(), Phase::Locking);
    }

    #[test]
    fn confirms_only_when_every_output_committed() {
        let mut m = LockMachine::default();
        let Verdict::Accepted(id) = m.request(&outs(&["A", "B"])) else {
            panic!()
        };
        assert!(!m.ready_to_confirm());
        assert!(m.surface_added(id, "A"));
        m.surface_committed("A");
        assert!(!m.ready_to_confirm());
        assert!(!m.confirmed());
        assert!(m.surface_added(id, "B"));
        // Present but no buffer yet.
        assert!(!m.ready_to_confirm());
        m.surface_committed("B");
        assert!(m.ready_to_confirm());
        assert!(m.confirmed());
        assert_eq!(m.phase(), Phase::Locked);
        // Once only.
        assert!(!m.confirmed());
    }

    #[test]
    fn never_confirms_without_outputs() {
        let mut m = LockMachine::default();
        m.request(&[]);
        assert!(!m.ready_to_confirm());
        assert!(!m.confirmed());
        assert_eq!(m.phase(), Phase::Locking);
    }

    #[test]
    fn new_output_while_locking_must_get_a_surface_too() {
        let mut m = LockMachine::default();
        let Verdict::Accepted(id) = m.request(&outs(&["A"])) else {
            panic!()
        };
        m.surface_added(id, "A");
        m.surface_committed("A");
        m.set_outputs(&outs(&["A", "B"]));
        assert!(!m.ready_to_confirm());
        m.surface_added(id, "B");
        m.surface_committed("B");
        assert!(m.ready_to_confirm());
    }

    #[test]
    fn removed_output_no_longer_blocks_confirmation() {
        let mut m = LockMachine::default();
        let Verdict::Accepted(id) = m.request(&outs(&["A", "B"])) else {
            panic!()
        };
        m.surface_added(id, "A");
        m.surface_committed("A");
        m.set_outputs(&outs(&["A"]));
        assert!(m.ready_to_confirm());
    }

    #[test]
    fn removed_surface_unconfirms_readiness() {
        let mut m = LockMachine::default();
        let Verdict::Accepted(id) = m.request(&outs(&["A"])) else {
            panic!()
        };
        m.surface_added(id, "A");
        m.surface_committed("A");
        m.surface_removed("A");
        assert!(!m.ready_to_confirm());
    }

    #[test]
    fn surfaces_of_strangers_and_unknown_outputs_are_refused() {
        let mut m = LockMachine::default();
        let Verdict::Accepted(id) = m.request(&outs(&["A"])) else {
            panic!()
        };
        assert!(!m.surface_added(id + 1, "A"));
        assert!(!m.surface_added(id, "Z"));
        assert!(m.surface_added(id, "A"));
        assert!(!m.surface_added(id, "A"));
        assert_eq!(m.surface_count(), 1);
    }

    #[test]
    fn second_request_is_denied_while_locking_or_locked() {
        let mut m = LockMachine::default();
        m.request(&outs(&["A"]));
        assert_eq!(m.request(&outs(&["A"])), Verdict::Denied);
        let (mut m, _) = locked(&["A"]);
        assert_eq!(m.request(&outs(&["A"])), Verdict::Denied);
        assert_eq!(m.phase(), Phase::Locked);
    }

    #[test]
    fn client_death_before_confirmation_unlocks() {
        let mut m = LockMachine::default();
        let Verdict::Accepted(id) = m.request(&outs(&["A"])) else {
            panic!()
        };
        assert_eq!(m.client_gone(id), Lost::Cancelled);
        assert_eq!(m.phase(), Phase::Unlocked);
        assert_eq!(m.surface_count(), 0);
    }

    #[test]
    fn client_death_while_locked_stays_locked() {
        let (mut m, id) = locked(&["A"]);
        assert_eq!(m.client_gone(id), Lost::Orphaned);
        assert_eq!(m.phase(), Phase::Locked);
        assert_eq!(m.owner(), None);
        assert_eq!(m.surface_count(), 0);
    }

    #[test]
    fn a_stranger_dying_changes_nothing() {
        let (mut m, id) = locked(&["A"]);
        assert_eq!(m.client_gone(id + 7), Lost::Nothing);
        assert_eq!(m.owner(), Some(id));
    }

    #[test]
    fn unlock_only_by_a_living_owner() {
        let mut m = LockMachine::default();
        assert!(!m.unlock());
        m.request(&outs(&["A"]));
        // Not confirmed yet: no unlock.
        assert!(!m.unlock());
        assert_eq!(m.phase(), Phase::Locking);

        let (mut m, id) = locked(&["A"]);
        m.client_gone(id);
        assert!(!m.unlock());
        assert_eq!(m.phase(), Phase::Locked);

        let (mut m, _) = locked(&["A"]);
        assert!(m.unlock());
        assert_eq!(m.phase(), Phase::Unlocked);
    }

    #[test]
    fn a_new_client_takes_over_a_dead_lock() {
        let (mut m, old) = locked(&["A"]);
        m.client_gone(old);
        let Verdict::Accepted(new) = m.request(&outs(&["A"])) else {
            panic!("takeover denied");
        };
        assert_ne!(new, old);
        // Still engaged while the newcomer has not confirmed.
        assert_eq!(m.phase(), Phase::Locking);
        assert!(!m.surface_added(old, "A"));
        assert!(m.surface_added(new, "A"));
        m.surface_committed("A");
        assert!(m.confirmed());
        assert_eq!(m.owner(), Some(new));
        assert!(m.unlock());
    }

    #[test]
    fn failed_takeover_falls_back_to_locked_not_unlocked() {
        let (mut m, old) = locked(&["A"]);
        m.client_gone(old);
        let Verdict::Accepted(new) = m.request(&outs(&["A"])) else {
            panic!()
        };
        assert_eq!(m.client_gone(new), Lost::Orphaned);
        assert_eq!(m.phase(), Phase::Locked);
        assert!(!m.unlock());
    }

    #[test]
    fn outputs_added_after_locking_show_black_until_served() {
        let (mut m, id) = locked(&["A"]);
        m.set_outputs(&outs(&["A", "B"]));
        assert_eq!(m.phase(), Phase::Locked);
        assert!(!m.has_surface("B"));
        assert!(m.surface_added(id, "B"));
        assert!(m.has_surface("B"));
    }

    #[test]
    fn can_lock_again_after_unlock() {
        let (mut m, _) = locked(&["A"]);
        assert!(m.unlock());
        assert!(matches!(m.request(&outs(&["A"])), Verdict::Accepted(_)));
        assert_eq!(m.phase(), Phase::Locking);
    }
}
