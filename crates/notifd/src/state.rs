//! The notification state machine: ids, replacement, expiry, stacking limits, history and
//! close reasons. Pure (no clock, no I/O): callers pass `now` in milliseconds on any
//! monotonic base and act on the returned [`Effect`]s.
//!
//! Model: at most `max_visible` notifications are *visible* (they get a toast and a
//! running expiry timer); the rest wait in `pending` and become visible, oldest first
//! (critical ones before others), when a slot frees. The timer of a notification starts
//! when it becomes visible. Visible order is arrival order (oldest first), so a new toast
//! never shifts the ones already on screen.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::hints::{Hints, Urgency};
use crate::markup::strip_markup;

/// Spec close reasons (`NotificationClosed.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    /// Closed by a `CloseNotification` call.
    Closed = 3,
    Undefined = 4,
}

impl CloseReason {
    pub fn code(self) -> u32 {
        self as u32
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub max_visible: usize,
    pub history: usize,
    /// Timeout for `expire_timeout == -1` on low and normal urgency. 0 means never.
    pub default_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_visible: 5,
            history: 50,
            default_timeout_ms: 5000,
        }
    }
}

/// Hands out ids from any thread. Ids start at 1 and are never reused; an id supplied by
/// a client through `replaces_id` is honoured and the counter moves past it.
#[derive(Debug)]
pub struct IdGen(AtomicU32);

impl Default for IdGen {
    fn default() -> Self {
        Self(AtomicU32::new(1))
    }
}

impl IdGen {
    /// The id to answer `Notify` with: `replaces_id` when nonzero, else a fresh one.
    pub fn assign(&self, replaces_id: u32) -> u32 {
        if replaces_id != 0 {
            self.0
                .fetch_max(replaces_id.saturating_add(1), Ordering::Relaxed);
            return replaces_id;
        }
        loop {
            let id = self.0.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
            // Wrapped around: skip 0.
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub key: String,
    pub label: String,
}

/// `Notify` arguments after the id was assigned.
#[derive(Clone, Debug)]
pub struct NotifyArgs {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    /// Flat `[key, label, key, label, ..]` as on the wire.
    pub actions: Vec<String>,
    pub hints: Hints,
    pub expire_timeout: i32,
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    /// Grows whenever the content changed (a replace); views rebuild on a new value.
    pub revision: u64,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    /// Markup already stripped.
    pub body: String,
    /// Named actions as buttons (the `default` pair is not among them).
    pub actions: Vec<Action>,
    pub has_default_action: bool,
    pub hints: Hints,
    pub urgency: Urgency,
    timeout_ms: Option<u64>,
    /// Deadline once visible.
    expire_at: Option<u64>,
}

impl Notification {
    pub fn expires_at(&self) -> Option<u64> {
        self.expire_at
    }
}

/// What the caller must do after a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Emit `NotificationClosed`.
    Closed { id: u32, reason: CloseReason },
    /// Emit `ActionInvoked`.
    ActionInvoked { id: u32, key: String },
}

#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub notification: Notification,
    pub reason: CloseReason,
    pub closed_at: u64,
}

const MAX_SUMMARY_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 2000;
const MAX_ACTIONS: usize = 8;

#[derive(Debug, Default)]
pub struct Center {
    cfg: Config,
    visible: Vec<Notification>,
    pending: VecDeque<Notification>,
    history: VecDeque<HistoryEntry>,
    revision: u64,
}

/// Splits the wire action list into the default action flag and named buttons. Pairs
/// with an empty key are skipped, a dangling key without a label is ignored.
pub fn parse_actions(flat: &[String]) -> (bool, Vec<Action>) {
    let mut has_default = false;
    let mut actions = Vec::new();
    for pair in flat.as_chunks::<2>().0 {
        let (key, label) = (&pair[0], &pair[1]);
        if key.is_empty() {
            continue;
        }
        if key == "default" {
            has_default = true;
        } else if actions.len() < MAX_ACTIONS && !actions.iter().any(|a: &Action| &a.key == key) {
            actions.push(Action {
                key: key.clone(),
                label: strip_markup(label),
            });
        }
    }
    (has_default, actions)
}

fn truncate_chars(s: String, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s,
    }
}

impl Center {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            ..Self::default()
        }
    }

    /// Effective auto-expiry: critical never; 0 never; -1 the configured default.
    pub fn timeout_for(&self, urgency: Urgency, expire_timeout: i32) -> Option<u64> {
        if urgency == Urgency::Critical {
            return None;
        }
        match expire_timeout {
            0 => None,
            t if t < 0 => (self.cfg.default_timeout_ms > 0).then_some(self.cfg.default_timeout_ms),
            t => Some(t as u64),
        }
    }

    fn build(&mut self, a: NotifyArgs) -> Notification {
        self.revision += 1;
        let (has_default, actions) = parse_actions(&a.actions);
        Notification {
            id: a.id,
            revision: self.revision,
            app_name: a.app_name,
            app_icon: a.app_icon,
            summary: truncate_chars(strip_markup(&a.summary), MAX_SUMMARY_CHARS),
            body: truncate_chars(strip_markup(&a.body), MAX_BODY_CHARS),
            actions,
            has_default_action: has_default,
            urgency: a.hints.urgency,
            timeout_ms: self.timeout_for(a.hints.urgency, a.expire_timeout),
            hints: a.hints,
            expire_at: None,
        }
    }

    /// Adds a notification, or replaces the one with the same id (in place when visible,
    /// in the queue when waiting, as a new one when it already went away).
    pub fn notify(&mut self, now: u64, args: NotifyArgs) {
        let n = self.build(args);
        if let Some(slot) = self.visible.iter_mut().find(|v| v.id == n.id) {
            let mut n = n;
            n.expire_at = n.timeout_ms.map(|t| now + t);
            *slot = n;
        } else if let Some(slot) = self.pending.iter_mut().find(|v| v.id == n.id) {
            *slot = n;
        } else if n.urgency == Urgency::Critical {
            // Criticals wait ahead of everything else.
            let at = self
                .pending
                .iter()
                .position(|p| p.urgency != Urgency::Critical)
                .unwrap_or(self.pending.len());
            self.pending.insert(at, n);
        } else {
            self.pending.push_back(n);
        }
        self.fill(now);
    }

    /// Moves waiting notifications into free visible slots.
    fn fill(&mut self, now: u64) {
        while self.visible.len() < self.cfg.max_visible {
            let Some(mut n) = self.pending.pop_front() else {
                break;
            };
            n.expire_at = n.timeout_ms.map(|t| now + t);
            self.visible.push(n);
        }
    }

    fn archive(&mut self, mut n: Notification, reason: CloseReason, now: u64) {
        n.hints.image = None;
        self.history.push_back(HistoryEntry {
            notification: n,
            reason,
            closed_at: now,
        });
        while self.history.len() > self.cfg.history {
            self.history.pop_front();
        }
    }

    /// Removes `id` wherever it is. Unknown ids are not an error and produce no effect.
    pub fn close(&mut self, now: u64, id: u32, reason: CloseReason) -> Vec<Effect> {
        let removed = if let Some(i) = self.visible.iter().position(|n| n.id == id) {
            Some(self.visible.remove(i))
        } else {
            self.pending
                .iter()
                .position(|n| n.id == id)
                .and_then(|i| self.pending.remove(i))
        };
        let Some(n) = removed else {
            return Vec::new();
        };
        self.archive(n, reason, now);
        self.fill(now);
        vec![Effect::Closed { id, reason }]
    }

    /// Invokes action `key`: emits `ActionInvoked`, then closes as dismissed unless the
    /// notification is `resident`. Unknown ids or keys do nothing.
    pub fn invoke(&mut self, now: u64, id: u32, key: &str) -> Vec<Effect> {
        let Some(n) = self.visible.iter().find(|n| n.id == id) else {
            return Vec::new();
        };
        let known = if key == "default" {
            n.has_default_action
        } else {
            n.actions.iter().any(|a| a.key == key)
        };
        if !known {
            return Vec::new();
        }
        let resident = n.hints.resident;
        let mut out = vec![Effect::ActionInvoked {
            id,
            key: key.to_string(),
        }];
        if !resident {
            out.extend(self.close(now, id, CloseReason::Dismissed));
        }
        out
    }

    /// A click on the toast body: the default action when there is one, else dismiss.
    pub fn activate(&mut self, now: u64, id: u32) -> Vec<Effect> {
        match self.visible.iter().find(|n| n.id == id) {
            Some(n) if n.has_default_action => self.invoke(now, id, "default"),
            Some(_) => self.close(now, id, CloseReason::Dismissed),
            None => Vec::new(),
        }
    }

    /// Closes every visible notification whose deadline passed, as expired.
    pub fn expire(&mut self, now: u64) -> Vec<Effect> {
        let due: Vec<u32> = self
            .visible
            .iter()
            .filter(|n| n.expire_at.is_some_and(|t| t <= now))
            .map(|n| n.id)
            .collect();
        due.into_iter()
            .flat_map(|id| self.close(now, id, CloseReason::Expired))
            .collect()
    }

    /// The earliest pending deadline, for arming the one expiry timer.
    pub fn next_deadline(&self) -> Option<u64> {
        self.visible.iter().filter_map(|n| n.expire_at).min()
    }

    /// Visible notifications, oldest first.
    pub fn visible(&self) -> &[Notification] {
        &self.visible
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn history(&self) -> impl ExactSizeIterator<Item = &HistoryEntry> {
        self.history.iter()
    }

    pub fn get(&self, id: u32) -> Option<&Notification> {
        self.visible.iter().find(|n| n.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(id: u32, timeout: i32) -> NotifyArgs {
        NotifyArgs {
            id,
            app_name: "app".into(),
            app_icon: String::new(),
            summary: format!("s{id}"),
            body: "body".into(),
            actions: vec![],
            hints: Hints::default(),
            expire_timeout: timeout,
        }
    }

    fn with_urgency(mut a: NotifyArgs, u: Urgency) -> NotifyArgs {
        a.hints.urgency = u;
        a
    }

    fn ids(c: &Center) -> Vec<u32> {
        c.visible().iter().map(|n| n.id).collect()
    }

    #[test]
    fn id_generation() {
        let g = IdGen::default();
        assert_eq!(g.assign(0), 1);
        assert_eq!(g.assign(0), 2);
        // A client-chosen id is honoured and never handed out again.
        assert_eq!(g.assign(10), 10);
        assert_eq!(g.assign(0), 11);
        assert_eq!(g.assign(3), 3);
        assert_eq!(g.assign(0), 12);
    }

    #[test]
    fn timeout_semantics() {
        let c = Center::new(Config::default());
        assert_eq!(c.timeout_for(Urgency::Normal, -1), Some(5000));
        assert_eq!(c.timeout_for(Urgency::Low, -1), Some(5000));
        assert_eq!(c.timeout_for(Urgency::Normal, 0), None);
        assert_eq!(c.timeout_for(Urgency::Normal, 1500), Some(1500));
        assert_eq!(c.timeout_for(Urgency::Critical, -1), None);
        assert_eq!(c.timeout_for(Urgency::Critical, 1500), None);
        let never = Center::new(Config {
            default_timeout_ms: 0,
            ..Config::default()
        });
        assert_eq!(never.timeout_for(Urgency::Normal, -1), None);
    }

    #[test]
    fn default_timeout_expires_as_reason_1() {
        let mut c = Center::default_for_test();
        c.notify(1000, args(1, -1));
        assert_eq!(c.next_deadline(), Some(6000));
        assert!(c.expire(5999).is_empty());
        let fx = c.expire(6000);
        assert_eq!(
            fx,
            vec![Effect::Closed {
                id: 1,
                reason: CloseReason::Expired
            }]
        );
        assert!(c.visible().is_empty());
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn zero_and_critical_never_expire() {
        let mut c = Center::default_for_test();
        c.notify(0, args(1, 0));
        c.notify(0, with_urgency(args(2, 10), Urgency::Critical));
        assert_eq!(c.next_deadline(), None);
        assert!(c.expire(u64::MAX).is_empty());
        assert_eq!(ids(&c), vec![1, 2]);
    }

    #[test]
    fn replace_in_place_keeps_slot_and_restarts_timer() {
        let mut c = Center::default_for_test();
        c.notify(0, args(1, 1000));
        c.notify(0, args(2, 1000));
        let rev = c.get(1).unwrap().revision;
        let mut again = args(1, 4000);
        again.summary = "changed".into();
        c.notify(500, again);
        assert_eq!(ids(&c), vec![1, 2]);
        assert_eq!(c.get(1).unwrap().summary, "changed");
        assert!(c.get(1).unwrap().revision > rev);
        assert_eq!(c.get(1).unwrap().expires_at(), Some(4500));
    }

    #[test]
    fn replacing_an_expired_id_creates_a_new_notification() {
        let mut c = Center::default_for_test();
        c.notify(0, args(1, 100));
        c.expire(100);
        c.notify(200, args(1, 100));
        assert_eq!(ids(&c), vec![1]);
    }

    #[test]
    fn visible_limit_queues_and_promotes_in_order() {
        let mut c = Center::new(Config {
            max_visible: 2,
            ..Config::default()
        });
        for id in 1..=4 {
            c.notify(0, args(id, 1000));
        }
        assert_eq!(ids(&c), vec![1, 2]);
        assert_eq!(c.pending_len(), 2);
        // Waiting ones have no running timer.
        assert_eq!(c.next_deadline(), Some(1000));
        c.expire(1000);
        assert_eq!(ids(&c), vec![3, 4]);
        // Their timers start when they become visible.
        assert_eq!(c.next_deadline(), Some(2000));
    }

    #[test]
    fn critical_jumps_the_queue() {
        let mut c = Center::new(Config {
            max_visible: 1,
            ..Config::default()
        });
        c.notify(0, args(1, 0));
        c.notify(0, args(2, 0));
        c.notify(0, with_urgency(args(3, 0), Urgency::Critical));
        c.notify(0, with_urgency(args(4, 0), Urgency::Critical));
        c.close(0, 1, CloseReason::Dismissed);
        assert_eq!(ids(&c), vec![3]);
        c.close(0, 3, CloseReason::Dismissed);
        assert_eq!(ids(&c), vec![4]);
        c.close(0, 4, CloseReason::Dismissed);
        assert_eq!(ids(&c), vec![2]);
    }

    #[test]
    fn replace_pending_updates_the_queue_entry() {
        let mut c = Center::new(Config {
            max_visible: 1,
            ..Config::default()
        });
        c.notify(0, args(1, 0));
        c.notify(0, args(2, 0));
        let mut r = args(2, 0);
        r.summary = "new".into();
        c.notify(0, r);
        assert_eq!(c.pending_len(), 1);
        c.close(0, 1, CloseReason::Closed);
        assert_eq!(c.get(2).unwrap().summary, "new");
    }

    #[test]
    fn close_reasons_and_unknown_ids() {
        let mut c = Center::default_for_test();
        c.notify(0, args(1, 0));
        c.notify(0, args(2, 0));
        assert!(c.close(0, 99, CloseReason::Closed).is_empty());
        assert_eq!(
            c.close(0, 1, CloseReason::Closed),
            vec![Effect::Closed {
                id: 1,
                reason: CloseReason::Closed
            }]
        );
        assert_eq!(CloseReason::Expired.code(), 1);
        assert_eq!(CloseReason::Dismissed.code(), 2);
        assert_eq!(CloseReason::Closed.code(), 3);
        assert_eq!(CloseReason::Undefined.code(), 4);
        // Closing twice is a no-op.
        assert!(c.close(0, 1, CloseReason::Closed).is_empty());
    }

    #[test]
    fn closing_a_pending_one_reports_it_too() {
        let mut c = Center::new(Config {
            max_visible: 1,
            ..Config::default()
        });
        c.notify(0, args(1, 0));
        c.notify(0, args(2, 0));
        let fx = c.close(0, 2, CloseReason::Closed);
        assert_eq!(fx.len(), 1);
        assert_eq!(c.pending_len(), 0);
    }

    fn actionable(id: u32) -> NotifyArgs {
        let mut a = args(id, 0);
        a.actions = ["default", "Open", "reply", "Reply", "dangling"]
            .map(String::from)
            .to_vec();
        a
    }

    #[test]
    fn action_pairs_parse() {
        let flat: Vec<String> = ["default", "Open", "a", "<b>A</b>", "", "x", "a", "dup", "k"]
            .map(String::from)
            .to_vec();
        let (def, acts) = parse_actions(&flat);
        assert!(def);
        assert_eq!(
            acts,
            vec![Action {
                key: "a".into(),
                label: "A".into()
            }]
        );
        assert_eq!(parse_actions(&[]), (false, vec![]));
    }

    #[test]
    fn invoking_an_action_emits_then_dismisses() {
        let mut c = Center::default_for_test();
        c.notify(0, actionable(1));
        assert_eq!(
            c.invoke(0, 1, "reply"),
            vec![
                Effect::ActionInvoked {
                    id: 1,
                    key: "reply".into()
                },
                Effect::Closed {
                    id: 1,
                    reason: CloseReason::Dismissed
                }
            ]
        );
        assert!(c.visible().is_empty());
    }

    #[test]
    fn unknown_action_is_ignored() {
        let mut c = Center::default_for_test();
        c.notify(0, actionable(1));
        assert!(c.invoke(0, 1, "nope").is_empty());
        assert!(c.invoke(0, 2, "reply").is_empty());
        let mut plain = Center::default_for_test();
        plain.notify(0, args(1, 0));
        assert!(plain.invoke(0, 1, "default").is_empty());
        assert_eq!(ids(&c), vec![1]);
    }

    #[test]
    fn resident_survives_its_action() {
        let mut c = Center::default_for_test();
        let mut a = actionable(1);
        a.hints.resident = true;
        c.notify(0, a);
        let fx = c.invoke(0, 1, "default");
        assert_eq!(fx.len(), 1);
        assert_eq!(ids(&c), vec![1]);
    }

    #[test]
    fn click_invokes_default_or_dismisses() {
        let mut c = Center::default_for_test();
        c.notify(0, actionable(1));
        c.notify(0, args(2, 0));
        let fx = c.activate(0, 1);
        assert_eq!(
            fx[0],
            Effect::ActionInvoked {
                id: 1,
                key: "default".into()
            }
        );
        assert_eq!(
            c.activate(0, 2),
            vec![Effect::Closed {
                id: 2,
                reason: CloseReason::Dismissed
            }]
        );
        assert!(c.activate(0, 2).is_empty());
    }

    #[test]
    fn history_ring_is_capped_and_ordered() {
        let mut c = Center::new(Config {
            history: 3,
            ..Config::default()
        });
        for id in 1..=5 {
            c.notify(0, args(id, 0));
            c.close(10 + id as u64, id, CloseReason::Dismissed);
        }
        let h: Vec<u32> = c.history().map(|e| e.notification.id).collect();
        assert_eq!(h, vec![3, 4, 5]);
        assert_eq!(c.history().last().unwrap().closed_at, 15);
        assert_eq!(c.history().last().unwrap().reason, CloseReason::Dismissed);
    }

    #[test]
    fn content_is_sanitised() {
        let mut c = Center::default_for_test();
        let mut a = args(1, 0);
        a.summary = "<b>Hi</b> &amp; bye".into();
        a.body = "x".repeat(MAX_BODY_CHARS + 50);
        c.notify(0, a);
        let n = c.get(1).unwrap();
        assert_eq!(n.summary, "Hi & bye");
        assert!(n.body.ends_with("..."));
        assert_eq!(n.body.chars().count(), MAX_BODY_CHARS + 3);
    }

    impl Center {
        fn default_for_test() -> Self {
            Self::new(Config::default())
        }
    }
}
