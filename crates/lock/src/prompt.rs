//! The password prompt as a pure state machine: typing, backspace, submit, failure
//! feedback and lockout backoff. Time is passed in (monotonic milliseconds), so the tests
//! need no clock. The only way out with `Verdict::Unlock` is a `Success` outcome that
//! arrives while an attempt is being checked.

use crate::auth::AuthOutcome;
use crate::secret::Secret;

/// Longest run of dots drawn; longer passwords just keep the full row.
pub const MAX_DOTS: usize = 32;
/// Failures that cost nothing extra beyond the authenticator's own delay.
const FREE_FAILURES: u32 = 2;
const BASE_LOCKOUT_MS: u64 = 1_000;
const MAX_LOCKOUT_MS: u64 = 30_000;

/// Keys the prompt understands; the caller maps toolkit events onto these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKey {
    Char(char),
    Backspace,
    /// Clears the whole entry.
    Escape,
    Enter,
}

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum Effect {
    /// Nothing visible changed.
    None,
    /// Dots or status changed: repaint.
    Changed,
    /// The user submitted this password; check it off the UI thread.
    Submit(Secret),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Unlock,
    Stay,
}

/// What the status line shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ready,
    Checking,
    Wrong,
    /// An authentication error that is not "wrong password".
    Failed,
    /// Input is refused for this many more whole seconds (rounded up).
    LockedOut(u64),
    /// Waiting for the compositor to confirm the lock; input is ignored.
    Waiting,
    Unlocking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Before `Locked`: typing is ignored so nothing can be submitted while the
    /// session is not yet locked.
    Waiting,
    Input,
    Checking,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feedback {
    None,
    Wrong,
    Failed,
}

/// Lockout after the `failures`-th consecutive failure, in ms: none for the first two,
/// then 1 s doubling up to 30 s.
pub fn lockout_ms(failures: u32) -> u64 {
    if failures <= FREE_FAILURES {
        return 0;
    }
    let shift = (failures - FREE_FAILURES - 1).min(10);
    (BASE_LOCKOUT_MS << shift).min(MAX_LOCKOUT_MS)
}

pub struct Prompt {
    secret: Secret,
    phase: Phase,
    feedback: Feedback,
    failures: u32,
    /// Input is refused until this instant.
    locked_until: u64,
}

impl Prompt {
    pub fn new() -> Self {
        Self {
            secret: Secret::new(),
            phase: Phase::Waiting,
            feedback: Feedback::None,
            failures: 0,
            locked_until: 0,
        }
    }

    /// The compositor confirmed the lock: typing starts to count.
    pub fn locked(&mut self) {
        if self.phase == Phase::Waiting {
            self.phase = Phase::Input;
        }
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    fn backoff_active(&self, now: u64) -> bool {
        now < self.locked_until
    }

    pub fn key(&mut self, key: PromptKey, now: u64) -> Effect {
        if self.phase != Phase::Input || self.backoff_active(now) {
            return Effect::None;
        }
        let before = (self.feedback, self.secret.chars());
        match key {
            PromptKey::Char(c) => {
                self.secret.push(c);
            }
            PromptKey::Backspace => {
                self.secret.pop();
            }
            PromptKey::Escape => self.secret.clear(),
            PromptKey::Enter => {
                if self.secret.is_empty() {
                    return Effect::None;
                }
                self.feedback = Feedback::None;
                self.phase = Phase::Checking;
                // The buffer is wiped the moment it is handed over.
                return Effect::Submit(self.secret.take());
            }
        }
        // Any edit dismisses the failure message.
        if self.secret.chars() != before.1 {
            self.feedback = Feedback::None;
        }
        if before == (self.feedback, self.secret.chars()) {
            Effect::None
        } else {
            Effect::Changed
        }
    }

    /// The authenticator answered the last submission.
    pub fn outcome(&mut self, outcome: &AuthOutcome, now: u64) -> Verdict {
        if self.phase != Phase::Checking {
            // An answer nobody asked for must never unlock.
            return Verdict::Stay;
        }
        match outcome {
            AuthOutcome::Success => {
                self.phase = Phase::Done;
                self.failures = 0;
                self.feedback = Feedback::None;
                Verdict::Unlock
            }
            AuthOutcome::Denied | AuthOutcome::Error(_) => {
                self.phase = Phase::Input;
                self.failures = self.failures.saturating_add(1);
                self.feedback = if matches!(outcome, AuthOutcome::Denied) {
                    Feedback::Wrong
                } else {
                    Feedback::Failed
                };
                self.locked_until = now + lockout_ms(self.failures);
                Verdict::Stay
            }
        }
    }

    pub fn status(&self, now: u64) -> Status {
        match self.phase {
            Phase::Waiting => Status::Waiting,
            Phase::Checking => Status::Checking,
            Phase::Done => Status::Unlocking,
            Phase::Input if self.backoff_active(now) => {
                Status::LockedOut((self.locked_until - now).div_ceil(1000))
            }
            Phase::Input => match self.feedback {
                Feedback::None => Status::Ready,
                Feedback::Wrong => Status::Wrong,
                Feedback::Failed => Status::Failed,
            },
        }
    }

    /// Milliseconds until the status line next changes by itself (a lockout second
    /// ticking or ending), or `None` when it is stable.
    pub fn next_change_ms(&self, now: u64) -> Option<u64> {
        if self.phase == Phase::Input && self.backoff_active(now) {
            let left = self.locked_until - now;
            // Wake at the next whole-second boundary of the remaining time.
            Some(match left % 1000 {
                0 => 1000,
                r => r,
            })
        } else {
            None
        }
    }

    /// The dots shown, never more than [`MAX_DOTS`].
    pub fn dots(&self) -> usize {
        self.secret.chars().min(MAX_DOTS)
    }
}

impl Default for Prompt {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Prompt {
        let mut p = Prompt::new();
        p.locked();
        p
    }

    fn type_str(p: &mut Prompt, s: &str, now: u64) {
        for c in s.chars() {
            p.key(PromptKey::Char(c), now);
        }
    }

    fn submit(p: &mut Prompt, now: u64) -> Secret {
        match p.key(PromptKey::Enter, now) {
            Effect::Submit(s) => s,
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn ignores_keys_until_locked() {
        let mut p = Prompt::new();
        assert_eq!(p.status(0), Status::Waiting);
        assert_eq!(p.key(PromptKey::Char('a'), 0), Effect::None);
        assert_eq!(p.dots(), 0);
        p.locked();
        assert_eq!(p.key(PromptKey::Char('a'), 0), Effect::Changed);
        assert_eq!(p.dots(), 1);
    }

    #[test]
    fn typing_backspace_and_escape() {
        let mut p = ready();
        type_str(&mut p, "abc", 0);
        assert_eq!(p.dots(), 3);
        assert_eq!(p.key(PromptKey::Backspace, 0), Effect::Changed);
        assert_eq!(p.dots(), 2);
        assert_eq!(p.key(PromptKey::Escape, 0), Effect::Changed);
        assert_eq!(p.dots(), 0);
        assert_eq!(p.key(PromptKey::Backspace, 0), Effect::None);
        assert_eq!(p.key(PromptKey::Escape, 0), Effect::None);
    }

    #[test]
    fn empty_enter_submits_nothing() {
        let mut p = ready();
        assert_eq!(p.key(PromptKey::Enter, 0), Effect::None);
        assert_eq!(p.status(0), Status::Ready);
    }

    #[test]
    fn submit_hands_over_and_wipes() {
        let mut p = ready();
        type_str(&mut p, "pw", 0);
        let s = submit(&mut p, 0);
        assert_eq!(s.as_bytes(), b"pw");
        assert_eq!(p.dots(), 0);
        assert_eq!(p.status(0), Status::Checking);
        // Typing while checking is dropped.
        assert_eq!(p.key(PromptKey::Char('x'), 0), Effect::None);
        assert_eq!(p.dots(), 0);
    }

    #[test]
    fn success_unlocks_only_after_a_submission() {
        let mut p = ready();
        assert_eq!(p.outcome(&AuthOutcome::Success, 0), Verdict::Stay);
        type_str(&mut p, "pw", 0);
        submit(&mut p, 0);
        assert_eq!(p.outcome(&AuthOutcome::Success, 5), Verdict::Unlock);
        assert_eq!(p.status(5), Status::Unlocking);
        // A second success cannot unlock again.
        assert_eq!(p.outcome(&AuthOutcome::Success, 6), Verdict::Stay);
    }

    #[test]
    fn success_before_locked_never_unlocks() {
        let mut p = Prompt::new();
        assert_eq!(p.outcome(&AuthOutcome::Success, 0), Verdict::Stay);
    }

    #[test]
    fn denied_and_error_never_unlock() {
        let mut p = ready();
        for (i, outcome) in [AuthOutcome::Denied, AuthOutcome::Error("x".into())]
            .iter()
            .enumerate()
        {
            type_str(&mut p, "pw", 0);
            submit(&mut p, 0);
            assert_eq!(p.outcome(outcome, 0), Verdict::Stay);
            assert_eq!(p.failures(), i as u32 + 1);
        }
        assert_eq!(p.status(0), Status::Failed);
    }

    #[test]
    fn wrong_password_feedback_clears_on_typing() {
        let mut p = ready();
        type_str(&mut p, "pw", 0);
        submit(&mut p, 0);
        p.outcome(&AuthOutcome::Denied, 0);
        assert_eq!(p.status(0), Status::Wrong);
        assert_eq!(p.key(PromptKey::Char('a'), 0), Effect::Changed);
        assert_eq!(p.status(0), Status::Ready);
    }

    #[test]
    fn lockout_schedule() {
        assert_eq!(lockout_ms(1), 0);
        assert_eq!(lockout_ms(2), 0);
        assert_eq!(lockout_ms(3), 1_000);
        assert_eq!(lockout_ms(4), 2_000);
        assert_eq!(lockout_ms(5), 4_000);
        assert_eq!(lockout_ms(8), 30_000);
        assert_eq!(lockout_ms(u32::MAX), 30_000);
    }

    #[test]
    fn lockout_refuses_input_until_it_ends() {
        let mut p = ready();
        for _ in 0..3 {
            type_str(&mut p, "x", 10_000);
            submit(&mut p, 10_000);
            p.outcome(&AuthOutcome::Denied, 10_000);
        }
        // Third failure at t=10s: locked out for 1 s.
        assert_eq!(p.status(10_000), Status::LockedOut(1));
        assert_eq!(p.key(PromptKey::Char('a'), 10_500), Effect::None);
        assert_eq!(p.dots(), 0, "keys during lockout are dropped, not buffered");
        assert_eq!(p.key(PromptKey::Enter, 10_999), Effect::None);
        assert_eq!(p.status(11_000), Status::Wrong);
        assert_eq!(p.key(PromptKey::Char('a'), 11_000), Effect::Changed);
    }

    #[test]
    fn lockout_countdown_rounds_up_and_wakes_on_seconds() {
        let mut p = ready();
        for i in 0..4 {
            let t = i * 100_000;
            type_str(&mut p, "x", t);
            submit(&mut p, t);
            p.outcome(&AuthOutcome::Denied, t);
        }
        // Fourth failure at t=300 s: 2 s.
        let t = 300_000;
        assert_eq!(p.status(t), Status::LockedOut(2));
        assert_eq!(p.status(t + 1), Status::LockedOut(2));
        assert_eq!(p.status(t + 1_001), Status::LockedOut(1));
        assert_eq!(p.next_change_ms(t), Some(1000));
        assert_eq!(p.next_change_ms(t + 1_500), Some(500));
        assert_eq!(p.next_change_ms(t + 2_000), None);
    }

    #[test]
    fn success_resets_failures() {
        let mut p = ready();
        type_str(&mut p, "x", 0);
        submit(&mut p, 0);
        p.outcome(&AuthOutcome::Denied, 0);
        type_str(&mut p, "y", 0);
        submit(&mut p, 0);
        p.outcome(&AuthOutcome::Success, 0);
        assert_eq!(p.failures(), 0);
    }

    #[test]
    fn dots_are_capped() {
        let mut p = ready();
        type_str(&mut p, &"a".repeat(100), 0);
        assert_eq!(p.dots(), MAX_DOTS);
    }
}
