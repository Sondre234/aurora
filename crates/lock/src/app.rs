//! The lock client: connects the pure prompt, the authenticator and the toolkit's
//! session-lock runner.
//!
//! Safety rules, all enforced here:
//! - `Runtime::unlock` is called in exactly one place, on `Verdict::Unlock`, which the
//!   prompt only returns for a `Success` that answers a submission made after `Locked`.
//! - Every other way out (errors, a finished lock, a dead connection, a panic) leaves the
//!   session locked: the compositor keeps it locked when the client goes away.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aurora_theme::Theme;
use aurora_ui::input::{Key, KeyEvent};
use aurora_ui::runtime::{
    App, Client, Error, Event, Output, Runtime, State, SurfaceId,
    calloop::{
        channel::{self, Channel, Sender},
        timer::{TimeoutAction, Timer},
    },
};
use aurora_ui::{TextSystem, UiEvent};

use crate::auth::{AuthOutcome, Authenticator, run_guarded};
use crate::clock::{self, LocalTime};
use crate::prompt::{Effect, Prompt, PromptKey, Verdict};
use crate::secret::Secret;
use crate::view::{self, Model};

/// How the process should exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Authenticated, session unlocked: exit 0.
    Unlocked,
    /// Anything else: exit non-zero, session still locked (or never locked).
    Failed,
}

pub struct LockApp {
    theme: Theme,
    user: String,
    prompt: Prompt,
    auth: Arc<dyn Authenticator>,
    text: TextSystem,
    surfaces: Vec<(u32, SurfaceId)>,
    started: Instant,
    results: Sender<AuthOutcome>,
    requested: bool,
    exit: Exit,
}

impl LockApp {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn model(&self) -> Model {
        let time = clock::now_local().unwrap_or(LocalTime {
            hour: 0,
            minute: 0,
            second: 0,
            weekday: 0,
            day: 1,
            month: 0,
        });
        Model {
            clock: time.clock(),
            date: time.date(),
            dots: self.prompt.dots(),
            status: self.prompt.status(self.now_ms()),
            // The toolkit does not report lock-key state, so there is no indicator yet.
            caps_lock: false,
        }
    }

    fn refresh(&self, rt: &mut Runtime<Self>) {
        let model = self.model();
        for (_, sid) in &self.surfaces {
            if let Some(ui) = rt.ui(*sid) {
                view::apply(ui, &self.theme, &model);
            }
        }
    }

    fn add_surface(&mut self, rt: &mut Runtime<Self>, output: &Output) {
        if !self.requested || self.surfaces.iter().any(|(id, _)| *id == output.id) {
            return;
        }
        let ui = view::build(self.text.clone(), &self.theme, &self.user, &self.model());
        match rt.create_lock_surface(output, ui) {
            Ok(sid) => {
                tracing::info!(
                    "lock-client: surface output={}",
                    output.name.as_deref().unwrap_or("?")
                );
                self.surfaces.push((output.id, sid));
            }
            Err(e) => tracing::error!("lock-client: surface failed: {e}"),
        }
    }

    fn remove_surface(&mut self, rt: &mut Runtime<Self>, output: &Output) {
        if let Some(i) = self.surfaces.iter().position(|(id, _)| *id == output.id) {
            let (_, sid) = self.surfaces.remove(i);
            rt.destroy(sid);
        }
    }

    fn key(&mut self, rt: &mut Runtime<Self>, k: KeyEvent) {
        let now = self.now_ms();
        let mut effects = Vec::new();
        let plain = !(k.mods.ctrl || k.mods.alt || k.mods.logo);
        match k.key {
            Key::Enter => effects.push(self.prompt.key(PromptKey::Enter, now)),
            Key::Backspace => effects.push(self.prompt.key(PromptKey::Backspace, now)),
            Key::Escape => effects.push(self.prompt.key(PromptKey::Escape, now)),
            // Ctrl+U clears the line like a terminal.
            Key::Char('u') if k.mods.ctrl && !k.mods.alt => {
                effects.push(self.prompt.key(PromptKey::Escape, now));
            }
            _ if plain => {
                for c in k.text.iter().flat_map(|t| t.chars()) {
                    effects.push(self.prompt.key(PromptKey::Char(c), now));
                }
            }
            _ => {}
        }
        let mut changed = false;
        for effect in effects {
            match effect {
                Effect::None => {}
                Effect::Changed => changed = true,
                Effect::Submit(secret) => {
                    changed = true;
                    self.submit(secret);
                }
            }
        }
        if changed {
            self.refresh(rt);
        }
    }

    /// Checks the password on a worker thread. PAM may sleep (failure delays) and must
    /// never stall painting or input.
    fn submit(&self, secret: Secret) {
        let auth = Arc::clone(&self.auth);
        let user = self.user.clone();
        let tx = self.results.clone();
        let spawned = std::thread::Builder::new()
            .name("aurora-lock-auth".into())
            .spawn({
                let tx = tx.clone();
                move || {
                    let outcome = run_guarded(&auth, &user, &secret);
                    drop(secret);
                    let _ = tx.send(outcome);
                }
            });
        if spawned.is_err() {
            let _ = tx.send(AuthOutcome::Error(
                "cannot start the authentication thread".into(),
            ));
        }
    }

    fn outcome(&mut self, rt: &mut Runtime<Self>, outcome: AuthOutcome) {
        let now = self.now_ms();
        if let AuthOutcome::Error(e) = &outcome {
            tracing::warn!("lock-client: authentication error: {e}");
        } else if outcome == AuthOutcome::Denied {
            tracing::info!("lock-client: authentication failed");
        }
        if self.prompt.outcome(&outcome, now) == Verdict::Unlock {
            self.unlock(rt);
        } else {
            self.refresh(rt);
        }
    }

    /// The only unlock in the program.
    fn unlock(&mut self, rt: &mut Runtime<Self>) {
        if !rt.is_locked() {
            tracing::error!("lock-client: refusing to unlock, the lock is not confirmed");
            return;
        }
        match rt.unlock() {
            Ok(()) => {
                tracing::info!("lock-client: unlocked");
                self.exit = Exit::Unlocked;
            }
            Err(e) => tracing::error!("lock-client: unlock failed: {e}"),
        }
        rt.quit();
    }

    /// Asks for the lock and covers every output known so far.
    fn start(&mut self, rt: &mut Runtime<Self>) {
        tracing::info!("lock-client: locking");
        if let Err(e) = rt.lock_session() {
            tracing::error!("lock-client: cannot lock: {e}");
            rt.quit();
            return;
        }
        self.requested = true;
        for output in rt.outputs().to_vec() {
            self.add_surface(rt, &output);
        }
    }

    /// Milliseconds until the screen next needs a refresh by itself.
    fn tick(&mut self, rt: &mut Runtime<Self>) -> u64 {
        self.refresh(rt);
        let to_minute = clock::now_local().map_or(30_000, |t| t.ms_to_next_minute());
        let wait = self
            .prompt
            .next_change_ms(self.now_ms())
            .map_or(to_minute, |w| w.min(to_minute));
        wait.clamp(50, 60_000)
    }
}

impl App for LockApp {
    fn event(&mut self, rt: &mut Runtime<Self>, event: Event) {
        match event {
            Event::Locked => {
                tracing::info!("lock-client: locked");
                self.prompt.locked();
                self.refresh(rt);
            }
            Event::LockFinished => {
                // Denied, or another client took the lock over. Never unlock from here.
                tracing::error!("lock-client: lock finished by the compositor, exiting");
                rt.quit();
            }
            Event::OutputAdded(o) => self.add_surface(rt, &o),
            Event::OutputRemoved(o) => self.remove_surface(rt, &o),
            Event::Ui {
                event: UiEvent::Key(k),
                ..
            } => self.key(rt, k),
            _ => {}
        }
    }
}

/// Connects, requests the lock and runs until unlocked or the lock ends. Returns how to
/// exit; the error case leaves the session locked.
pub fn run(theme: Theme, user: String, auth: Arc<dyn Authenticator>) -> Result<Exit, Error> {
    let text = TextSystem::new();
    if !text.has_fonts() {
        tracing::warn!("lock-client: no fonts installed, the screen will show no text");
    }
    let (results, channel): (Sender<AuthOutcome>, Channel<AuthOutcome>) = channel::channel();
    let app = LockApp {
        theme,
        user,
        prompt: Prompt::new(),
        auth,
        text: text.clone(),
        surfaces: Vec::new(),
        started: Instant::now(),
        results,
        requested: false,
        exit: Exit::Failed,
    };
    let client = Client::connect(text, app)?;
    let handle = client.handle();

    handle
        .insert_source(channel, |event, _, state: &mut State<LockApp>| {
            if let channel::Event::Msg(outcome) = event {
                state.app.outcome(&mut state.rt, outcome);
            }
        })
        .map_err(|e| Error::Loop(e.to_string()))?;
    handle
        .insert_source(
            Timer::from_duration(Duration::from_secs(1)),
            |_, _, state: &mut State<LockApp>| {
                let wait = state.app.tick(&mut state.rt);
                TimeoutAction::ToDuration(Duration::from_millis(wait))
            },
        )
        .map_err(|e| Error::Loop(e.to_string()))?;

    // Request the lock from inside the loop, where the app and the runtime are both at hand.
    handle
        .insert_source(Timer::immediate(), |_, _, state: &mut State<LockApp>| {
            state.app.start(&mut state.rt);
            TimeoutAction::Drop
        })
        .map_err(|e| Error::Loop(e.to_string()))?;
    let app = client.run()?;
    Ok(app.exit)
}
