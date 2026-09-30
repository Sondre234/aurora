//! Service supervision: the long-lived helpers from `[services.<name>]` (bar, launcher,
//! notifier, lock client). The compositor starts them, notices when they end, restarts them
//! with backoff and stops them at shutdown. Everything runs on the event loop: one reaper
//! thread per child posts the exit into a calloop channel, and a restart is a calloop timer.
//!
//! The decisions (`backoff_delay`, `next_consecutive`, `restart_allowed`, `plan`) are pure
//! and unit tested; `Aurora`'s methods below only apply them.
use std::{
    os::unix::process::ExitStatusExt,
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use smithay::reexports::calloop::{
    RegistrationToken,
    channel::{self, Channel, Sender},
    timer::{TimeoutAction, Timer},
};

use crate::{
    config::{RestartPolicy, ServiceSpec},
    spawn::spawn_service,
    state::Aurora,
};

/// A run at least this long counts as healthy: the next failure restarts after the initial
/// backoff again.
pub const STABLE_RUN: Duration = Duration::from_secs(10);

/// How long a service gets to leave after SIGTERM before SIGKILL, while the compositor runs.
const KILL_AFTER: Duration = Duration::from_secs(5);

/// Shutdown waits this long for services to leave before killing them.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1000);

/// How a service process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitInfo {
    fn from_status(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            signal: status.signal(),
        }
    }

    pub fn success(self) -> bool {
        self.code == Some(0)
    }

    /// The exit code, or minus the signal number, for the log line.
    pub fn label(self) -> i32 {
        self.code.or(self.signal.map(|s| -s)).unwrap_or(-1)
    }
}

/// Delay before restart number `consecutive` (1 is the first) of a quickly failing service:
/// `initial`, doubling each time, never above `max`.
pub fn backoff_delay(initial_ms: u32, max_ms: u32, consecutive: u32) -> Duration {
    let shift = consecutive.saturating_sub(1).min(20);
    let ms = u64::from(initial_ms).saturating_mul(1 << shift);
    Duration::from_millis(ms.min(u64::from(max_ms)))
}

/// The failure streak after a run of `ran_for` that just ended: a healthy run starts over.
pub fn next_consecutive(prev: u32, ran_for: Duration) -> u32 {
    if ran_for >= STABLE_RUN {
        1
    } else {
        prev.saturating_add(1)
    }
}

/// Whether the supervisor starts the service again after `exit`. `locked` is whether the
/// session lock is engaged: the `lock` service only comes back while it is, so a lock client
/// that crashes before locking does not lock the user out, and one that dies mid-lock gets
/// replaced (the session stays locked either way, a restart can never unlock it).
pub fn restart_allowed(spec: &ServiceSpec, exit: ExitInfo, locked: bool) -> bool {
    let by_policy = match spec.restart {
        RestartPolicy::Never => false,
        RestartPolicy::OnFailure => !exit.success(),
        RestartPolicy::Always => true,
    };
    by_policy && spec.enabled && (locked || spec.name != LOCK_SERVICE)
}

/// The service the `lock` action starts.
pub const LOCK_SERVICE: &str = "lock";

/// What a config reload does to the supervisor.
#[derive(Debug, PartialEq, Eq)]
pub enum Op {
    Add(ServiceSpec),
    /// New settings for a known service. A running process is left alone; the command and
    /// the policy apply from the next start.
    Update(ServiceSpec),
    Start(String),
    Stop(String),
    Remove(String),
}

fn startable(s: &ServiceSpec) -> bool {
    s.enabled && s.autostart
}

/// Turns the previously applied specs and the new ones into operations. Services whose
/// spec did not change produce nothing, so a reload can neither double-start nor restart
/// a healthy one; a start is only planned for a service that was not startable before.
pub fn plan(old: &[ServiceSpec], new: &[ServiceSpec]) -> Vec<Op> {
    let mut ops = Vec::new();
    for n in new {
        match old.iter().find(|o| o.name == n.name) {
            None => {
                ops.push(Op::Add(n.clone()));
                if startable(n) {
                    ops.push(Op::Start(n.name.clone()));
                }
            }
            Some(o) => {
                if o != n {
                    ops.push(Op::Update(n.clone()));
                }
                if o.enabled && !n.enabled {
                    ops.push(Op::Stop(n.name.clone()));
                } else if startable(n) && !startable(o) {
                    ops.push(Op::Start(n.name.clone()));
                }
            }
        }
    }
    for o in old {
        if !new.iter().any(|n| n.name == o.name) {
            ops.push(Op::Remove(o.name.clone()));
        }
    }
    ops
}

/// A reaper thread's report.
pub struct Exit {
    name: String,
    ticket: u64,
    info: ExitInfo,
}

enum Run {
    Stopped,
    Running {
        pid: u32,
        ticket: u64,
        started: Instant,
        /// Set by the reaper once the child is gone, so shutdown can wait without the loop.
        gone: Arc<AtomicBool>,
        /// SIGTERM was sent on purpose: its exit is not a failure to recover from.
        stopping: bool,
    },
    /// Waiting out the backoff; the timer must be removed if the service is stopped first.
    Waiting {
        ticket: u64,
        timer: RegistrationToken,
    },
}

struct Svc {
    spec: ServiceSpec,
    run: Run,
    /// Failures in a row, see `next_consecutive`.
    consecutive: u32,
    /// Restarts since the compositor started.
    restarts: u32,
    /// Dropped from the config: forget it once its process is gone.
    removed: bool,
}

pub struct Services {
    list: Vec<Svc>,
    tx: Sender<Exit>,
    next_ticket: u64,
}

impl Services {
    pub fn new() -> (Self, Channel<Exit>) {
        let (tx, rx) = channel::channel();
        (
            Self {
                list: Vec::new(),
                tx,
                next_ticket: 0,
            },
            rx,
        )
    }

    /// The specs as last applied, without services that are on their way out.
    fn specs(&self) -> Vec<ServiceSpec> {
        self.list
            .iter()
            .filter(|s| !s.removed)
            .map(|s| s.spec.clone())
            .collect()
    }

    fn find(&mut self, name: &str) -> Option<&mut Svc> {
        self.list.iter_mut().find(|s| s.spec.name == name)
    }

    fn ticket(&mut self) -> u64 {
        self.next_ticket += 1;
        self.next_ticket
    }
}

fn signal_group(pid: u32, signal: i32) {
    // Children call setsid, so the pid is also their process group. Never 0 or 1, which
    // would address our own group or every process.
    if pid > 1
        && let Ok(pid) = i32::try_from(pid)
    {
        // Safety: plain kill(2) on a process group this compositor created.
        unsafe { libc::kill(-pid, signal) };
    }
}

impl Aurora {
    /// Registers the exit channel with the loop. Called once from `Aurora::new`.
    pub fn services_source(
        handle: &smithay::reexports::calloop::LoopHandle<'static, Aurora>,
        rx: Channel<Exit>,
    ) {
        let result = handle.insert_source(rx, |event, _, state| {
            if let channel::Event::Msg(exit) = event {
                state.service_exited(exit);
            }
        });
        if let Err(err) = result {
            tracing::error!(%err, "failed to register the service exit channel");
        }
    }

    /// Startup: applies the configured services (autostart ones start).
    pub fn services_init(&mut self) {
        let new = self.config.services.clone();
        self.services_apply(&new);
    }

    /// After a config reload: brings the supervisor to the new specs without touching
    /// services that did not change.
    pub fn services_reload(&mut self) {
        let new = self.config.services.clone();
        self.services_apply(&new);
    }

    fn services_apply(&mut self, new: &[ServiceSpec]) {
        let old = self.services.specs();
        for op in plan(&old, new) {
            match op {
                Op::Add(spec) => match self.services.find(&spec.name) {
                    // Removed and added back while its process was still leaving.
                    Some(svc) => {
                        svc.spec = spec;
                        svc.removed = false;
                    }
                    None => self.services.list.push(Svc {
                        spec,
                        run: Run::Stopped,
                        consecutive: 0,
                        restarts: 0,
                        removed: false,
                    }),
                },
                Op::Update(spec) => {
                    if let Some(svc) = self.services.find(&spec.name) {
                        let running = !matches!(svc.run, Run::Stopped);
                        if running && svc.spec.command != spec.command {
                            tracing::info!(
                                "service: command changed name={}, applies on next start",
                                spec.name
                            );
                        }
                        svc.spec = spec;
                        svc.removed = false;
                    }
                }
                Op::Start(name) => {
                    self.service_start(&name);
                }
                Op::Stop(name) => self.service_stop(&name),
                Op::Remove(name) => {
                    self.service_stop(&name);
                    if let Some(svc) = self.services.find(&name) {
                        svc.removed = true;
                    }
                    self.services_prune();
                }
            }
        }
    }

    /// Forgets removed services that have no process left.
    fn services_prune(&mut self) {
        self.services
            .list
            .retain(|s| !(s.removed && matches!(s.run, Run::Stopped)));
    }

    /// Starts `name` unless it is running or waiting out a backoff. Returns whether a
    /// process was started.
    pub fn service_start(&mut self, name: &str) -> bool {
        let Some(svc) = self.services.find(name) else {
            return false;
        };
        if !matches!(svc.run, Run::Stopped) || !svc.spec.enabled {
            return false;
        }
        let command = svc.spec.command.clone();
        let env = self.base_env();
        let env: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        let ticket = self.services.ticket();
        let tx = self.services.tx.clone();
        let gone = Arc::new(AtomicBool::new(false));

        let started = spawn_service(&command, &env)
            .map_err(|e| e.to_string())
            .and_then(|mut child| {
                let pid = child.id();
                let (svc_name, flag) = (name.to_string(), gone.clone());
                std::thread::Builder::new()
                    .name("reaper".into())
                    .spawn(move || {
                        let info = child.wait().map_or(
                            ExitInfo {
                                code: None,
                                signal: None,
                            },
                            ExitInfo::from_status,
                        );
                        flag.store(true, Ordering::SeqCst);
                        let _ = tx.send(Exit {
                            name: svc_name,
                            ticket,
                            info,
                        });
                    })
                    .map(|_| pid)
                    .map_err(|e| {
                        // The child moved into the failed closure: do not leave it running.
                        signal_group(pid, libc::SIGKILL);
                        format!("cannot start a reaper thread: {e}")
                    })
            });
        match started {
            Ok(pid) => {
                if let Some(svc) = self.services.find(name) {
                    svc.run = Run::Running {
                        pid,
                        ticket,
                        started: Instant::now(),
                        gone,
                        stopping: false,
                    };
                }
                tracing::info!("service: started name={name} pid={pid}");
                true
            }
            Err(err) => {
                tracing::warn!("service: cannot start name={name}: {err}");
                let info = ExitInfo {
                    code: Some(-1),
                    signal: None,
                };
                self.service_ended(name, info, Duration::ZERO);
                false
            }
        }
    }

    /// `name` should not run: SIGTERM to a running process (SIGKILL after a grace period), a
    /// pending restart is cancelled. Never restarts afterwards.
    fn service_stop(&mut self, name: &str) {
        let handle = self.handle.clone();
        let Some(svc) = self.services.find(name) else {
            return;
        };
        match &mut svc.run {
            Run::Stopped => {}
            Run::Waiting { timer, .. } => {
                handle.remove(*timer);
                svc.run = Run::Stopped;
                tracing::info!("service: stopped name={name}");
            }
            Run::Running {
                pid,
                ticket,
                stopping,
                ..
            } => {
                if *stopping {
                    return;
                }
                *stopping = true;
                let (pid, ticket) = (*pid, *ticket);
                tracing::info!("service: stopping name={name} pid={pid}");
                signal_group(pid, libc::SIGTERM);
                let n = name.to_string();
                let killer =
                    handle.insert_source(Timer::from_duration(KILL_AFTER), move |_, _, state| {
                        state.service_kill_if_still(&n, ticket);
                        TimeoutAction::Drop
                    });
                if let Err(err) = killer {
                    tracing::warn!(%err, "service: cannot arm the kill timer");
                }
            }
        }
    }

    fn service_kill_if_still(&mut self, name: &str, ticket: u64) {
        let Some(svc) = self.services.find(name) else {
            return;
        };
        if let Run::Running {
            pid,
            ticket: t,
            gone,
            ..
        } = &svc.run
            && *t == ticket
            && !gone.load(Ordering::SeqCst)
        {
            tracing::warn!("service: name={name} ignored SIGTERM, killing");
            signal_group(*pid, libc::SIGKILL);
        }
    }

    /// A reaper reported a process exit.
    fn service_exited(&mut self, exit: Exit) {
        let Some(svc) = self.services.find(&exit.name) else {
            return;
        };
        let Run::Running {
            ticket, started, ..
        } = &svc.run
        else {
            return;
        };
        if *ticket != exit.ticket {
            return; // an older process of the same service
        }
        let ran_for = started.elapsed();
        self.service_ended(&exit.name, exit.info, ran_for);
    }

    /// The process of `name` is gone (or never started): decide about a restart.
    fn service_ended(&mut self, name: &str, info: ExitInfo, ran_for: Duration) {
        let locked = self.is_locked();
        let handle = self.handle.clone();
        let Some(svc) = self.services.find(name) else {
            return;
        };
        let stopped_on_purpose = matches!(svc.run, Run::Running { stopping: true, .. });
        svc.run = Run::Stopped;
        svc.consecutive = next_consecutive(svc.consecutive, ran_for);
        let restart =
            !stopped_on_purpose && !svc.removed && restart_allowed(&svc.spec, info, locked);
        if !restart {
            tracing::info!(
                "service: exited name={name} code={} restart=none",
                info.label()
            );
            self.services_prune();
            return;
        }
        let delay = backoff_delay(
            svc.spec.backoff_ms,
            svc.spec.max_backoff_ms,
            svc.consecutive,
        );
        let ticket = self.services.ticket();
        let n = name.to_string();
        let timer = handle.insert_source(Timer::from_duration(delay), move |_, _, state| {
            state.service_restart_due(&n, ticket);
            TimeoutAction::Drop
        });
        let Some(svc) = self.services.find(name) else {
            return;
        };
        match timer {
            Ok(timer) => {
                svc.run = Run::Waiting { ticket, timer };
                svc.restarts += 1;
                tracing::info!(
                    "service: exited name={name} code={} restart={}",
                    info.label(),
                    delay.as_millis()
                );
            }
            Err(err) => {
                tracing::warn!(%err, "service: cannot arm the restart timer name={name}");
                tracing::info!(
                    "service: exited name={name} code={} restart=none",
                    info.label()
                );
            }
        }
    }

    fn service_restart_due(&mut self, name: &str, ticket: u64) {
        let Some(svc) = self.services.find(name) else {
            return;
        };
        match svc.run {
            // Only the timer that is still the current one may start it.
            Run::Waiting { ticket: t, .. } if t == ticket => svc.run = Run::Stopped,
            _ => return,
        }
        self.service_start(name);
    }

    /// The `lock` action: starts the lock service unless the session is locked already or
    /// its client is running.
    pub fn lock_action(&mut self) {
        if self.is_locked() {
            tracing::info!("lock: already locked");
            return;
        }
        let known = self
            .services
            .list
            .iter()
            .any(|s| s.spec.name == LOCK_SERVICE && s.spec.enabled && !s.removed);
        if !known {
            tracing::warn!(
                "lock: no enabled [services.{LOCK_SERVICE}] configured, nothing to start"
            );
            return;
        }
        if let Some(svc) = self.services.find(LOCK_SERVICE) {
            // An explicit request starts over, whatever a past crash streak was.
            svc.consecutive = 0;
            if let Run::Waiting { timer, .. } = svc.run {
                self.handle.remove(timer);
                svc.run = Run::Stopped;
            }
        }
        if !self.service_start(LOCK_SERVICE) {
            tracing::info!("lock: the lock service is already running");
        }
    }

    /// Whether `lock` is configured, for the IPC `Lock` request.
    pub fn lock_service_configured(&self) -> bool {
        self.services
            .list
            .iter()
            .any(|s| s.spec.name == LOCK_SERVICE && s.spec.enabled && !s.removed)
    }

    /// After the loop ends: stops every service so none outlives the compositor. SIGTERM to
    /// each group, a short wait, then SIGKILL for whatever is left.
    pub fn services_shutdown(&mut self) {
        let handle = self.handle.clone();
        let mut pending = Vec::new();
        for svc in &mut self.services.list {
            match &svc.run {
                Run::Waiting { timer, .. } => {
                    handle.remove(*timer);
                    svc.run = Run::Stopped;
                }
                Run::Running { pid, gone, .. } if !gone.load(Ordering::SeqCst) => {
                    tracing::info!("service: stopping name={} pid={pid}", svc.spec.name);
                    signal_group(*pid, libc::SIGTERM);
                    pending.push((svc.spec.name.clone(), *pid, gone.clone()));
                }
                _ => {}
            }
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while pending
            .iter()
            .any(|(_, _, gone)| !gone.load(Ordering::SeqCst))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        for (name, pid, gone) in &pending {
            if !gone.load(Ordering::SeqCst) {
                tracing::warn!("service: name={name} still running at shutdown, killing");
                signal_group(*pid, libc::SIGKILL);
            }
        }
        for svc in &mut self.services.list {
            svc.run = Run::Stopped;
        }
    }

    /// `dump: service <name> pid=<pid|none> restarts=<n>` per service, by name.
    pub fn dump_services(&self) {
        let mut list: Vec<_> = self.services.list.iter().collect();
        list.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        for svc in list {
            let pid = match svc.run {
                Run::Running { pid, .. } => pid.to_string(),
                _ => "none".to_string(),
            };
            tracing::info!(
                "dump: service {} pid={pid} restarts={}",
                svc.spec.name,
                svc.restarts
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ServiceSpec {
        ServiceSpec {
            name: name.into(),
            command: format!("run-{name}"),
            enabled: true,
            autostart: true,
            restart: RestartPolicy::OnFailure,
            backoff_ms: 500,
            max_backoff_ms: 30_000,
        }
    }

    const FAIL: ExitInfo = ExitInfo {
        code: Some(1),
        signal: None,
    };
    const CLEAN: ExitInfo = ExitInfo {
        code: Some(0),
        signal: None,
    };
    const KILLED: ExitInfo = ExitInfo {
        code: None,
        signal: Some(9),
    };

    #[test]
    fn backoff_doubles_and_caps() {
        let ms = |c| backoff_delay(500, 30_000, c).as_millis();
        assert_eq!(
            [ms(1), ms(2), ms(3), ms(4)],
            [500, 1000, 2000, 4000],
            "doubles per failure"
        );
        assert_eq!(ms(7), 30_000);
        assert_eq!(ms(200), 30_000, "a huge streak does not overflow");
        assert_eq!(ms(0), 500, "streak 0 behaves like the first");
        assert_eq!(backoff_delay(9_000, 100, 1).as_millis(), 100, "max wins");
    }

    #[test]
    fn a_healthy_run_resets_the_streak() {
        assert_eq!(next_consecutive(0, Duration::from_millis(50)), 1);
        assert_eq!(next_consecutive(3, Duration::from_secs(1)), 4);
        assert_eq!(next_consecutive(5, STABLE_RUN), 1);
        assert_eq!(next_consecutive(u32::MAX, Duration::ZERO), u32::MAX);
    }

    #[test]
    fn restart_policy_matrix() {
        let with = |restart| ServiceSpec {
            restart,
            ..spec("shell")
        };
        let allowed = |r, e| restart_allowed(&with(r), e, false);
        assert!(!allowed(RestartPolicy::Never, FAIL));
        assert!(!allowed(RestartPolicy::Never, CLEAN));
        assert!(allowed(RestartPolicy::OnFailure, FAIL));
        assert!(allowed(RestartPolicy::OnFailure, KILLED));
        assert!(!allowed(RestartPolicy::OnFailure, CLEAN));
        assert!(allowed(RestartPolicy::Always, CLEAN));
        assert!(allowed(RestartPolicy::Always, FAIL));
        let mut off = with(RestartPolicy::Always);
        off.enabled = false;
        assert!(!restart_allowed(&off, FAIL, false));
    }

    #[test]
    fn the_lock_service_only_returns_while_locked() {
        let lock = ServiceSpec {
            restart: RestartPolicy::Always,
            autostart: false,
            ..spec(LOCK_SERVICE)
        };
        assert!(!restart_allowed(&lock, FAIL, false));
        assert!(restart_allowed(&lock, FAIL, true));
    }

    #[test]
    fn exit_labels() {
        assert_eq!(CLEAN.label(), 0);
        assert_eq!(FAIL.label(), 1);
        assert_eq!(KILLED.label(), -9);
        assert!(CLEAN.success() && !FAIL.success() && !KILLED.success());
    }

    #[test]
    fn startup_adds_and_starts_the_autostart_services() {
        let mut lock = spec("lock");
        lock.autostart = false;
        let mut off = spec("off");
        off.enabled = false;
        let new = [spec("shell"), lock.clone(), off.clone()];
        assert_eq!(
            plan(&[], &new),
            vec![
                Op::Add(spec("shell")),
                Op::Start("shell".into()),
                Op::Add(lock),
                Op::Add(off),
            ]
        );
    }

    #[test]
    fn an_unchanged_reload_does_nothing() {
        let specs = [spec("shell"), spec("bar")];
        assert!(plan(&specs, &specs).is_empty());
    }

    #[test]
    fn a_changed_command_updates_without_a_restart() {
        let old = [spec("shell")];
        let mut changed = spec("shell");
        changed.command = "other".into();
        assert_eq!(plan(&old, &[changed.clone()]), vec![Op::Update(changed)]);
    }

    #[test]
    fn policy_changes_do_not_start_or_stop() {
        let old = [spec("shell")];
        let mut changed = spec("shell");
        changed.restart = RestartPolicy::Always;
        changed.backoff_ms = 100;
        assert_eq!(plan(&old, &[changed.clone()]), vec![Op::Update(changed)]);
    }

    #[test]
    fn disabling_stops_and_enabling_starts() {
        let on = spec("shell");
        let mut off = spec("shell");
        off.enabled = false;
        assert_eq!(
            plan(std::slice::from_ref(&on), std::slice::from_ref(&off)),
            vec![Op::Update(off.clone()), Op::Stop("shell".into())]
        );
        assert_eq!(
            plan(&[off], std::slice::from_ref(&on)),
            vec![Op::Update(on), Op::Start("shell".into())]
        );
    }

    #[test]
    fn removed_services_are_removed_and_new_ones_added() {
        let old = [spec("a"), spec("b")];
        let new = [spec("b"), spec("c")];
        assert_eq!(
            plan(&old, &new),
            vec![
                Op::Add(spec("c")),
                Op::Start("c".into()),
                Op::Remove("a".into()),
            ]
        );
    }

    #[test]
    fn autostart_turned_on_starts_a_stopped_on_demand_service() {
        let mut demand = spec("lock");
        demand.autostart = false;
        let auto = spec("lock");
        assert_eq!(
            plan(&[demand], std::slice::from_ref(&auto)),
            vec![Op::Update(auto), Op::Start("lock".into())]
        );
    }
}
