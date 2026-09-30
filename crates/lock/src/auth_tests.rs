//! Authenticator behaviour with fakes. Real PAM is never invoked from tests.

use std::sync::Arc;

use crate::auth::{AuthOutcome, Authenticator, Stub, run_guarded};
use crate::prompt::{Effect, Prompt, PromptKey, Verdict};
use crate::secret::Secret;

/// Accepts exactly one password.
struct Fake(&'static str);

impl Authenticator for Fake {
    fn authenticate(&self, _: &str, secret: &Secret) -> AuthOutcome {
        if secret.as_bytes() == self.0.as_bytes() {
            AuthOutcome::Success
        } else {
            AuthOutcome::Denied
        }
    }

    fn describe(&self) -> String {
        "fake".into()
    }
}

struct Panicky;

impl Authenticator for Panicky {
    fn authenticate(&self, _: &str, _: &Secret) -> AuthOutcome {
        panic!("boom");
    }

    fn describe(&self) -> String {
        "panicky".into()
    }
}

fn secret(s: &str) -> Secret {
    let mut out = Secret::new();
    s.chars().for_each(|c| {
        out.push(c);
    });
    out
}

/// Types `pw` into the prompt, runs the authenticator like the app does and returns the
/// verdict.
fn attempt(p: &mut Prompt, auth: &Arc<dyn Authenticator>, pw: &str, now: u64) -> Verdict {
    for c in pw.chars() {
        p.key(PromptKey::Char(c), now);
    }
    let Effect::Submit(s) = p.key(PromptKey::Enter, now) else {
        panic!("no submission");
    };
    let outcome = run_guarded(auth, "user", &s);
    p.outcome(&outcome, now)
}

#[test]
fn wrong_then_right_password() {
    let auth: Arc<dyn Authenticator> = Arc::new(Fake("correct"));
    let mut p = Prompt::new();
    p.locked();
    assert_eq!(attempt(&mut p, &auth, "wrong", 0), Verdict::Stay);
    assert_eq!(attempt(&mut p, &auth, "correct", 10_000), Verdict::Unlock);
}

#[test]
fn a_panicking_authenticator_fails_closed() {
    let auth: Arc<dyn Authenticator> = Arc::new(Panicky);
    let outcome = run_guarded(&auth, "user", &secret("x"));
    assert!(matches!(outcome, AuthOutcome::Error(_)));
    let mut p = Prompt::new();
    p.locked();
    assert_eq!(attempt(&mut p, &auth, "x", 0), Verdict::Stay);
}

#[test]
fn the_stub_never_accepts_anything() {
    let auth: Arc<dyn Authenticator> = Arc::new(Stub);
    for pw in ["", "x", "correct"] {
        assert!(matches!(
            run_guarded(&auth, "user", &secret(pw)),
            AuthOutcome::Error(_)
        ));
    }
}
