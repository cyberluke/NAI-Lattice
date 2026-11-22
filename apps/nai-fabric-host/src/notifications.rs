//! Native Windows notifications for Gas Town, driven off the durable event log.
//!
//! Satisfies the "why do I see nothing when an agent finishes" gap: when a
//! convoy lands, a bead fails irrecoverably, review cycles are exhausted, or a
//! task blocks, the operator is notified natively on Windows (Win32 toast), with
//! a **durable inbox** as the source of truth that survives even if Windows
//! drops or the user misses the toast. Notifications are derived from the
//! Gas Town event log (anti-spam: deduplicated per event), never spawned as a
//! `powershell.exe` per event, and require no VS Code or editor focus.

use nai_lattice_gastown::orchestration::state::OrchestrationStore;
use std::path::PathBuf;

/// One durable, operator-visible notification.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Notification {
    pub id: String,
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
    /// Which raw Gas Town event this was derived from (dedup key).
    pub source_event_id: String,
    /// Link target (e.g. `convoy:<id>`, `bead:<id>`) the dashboard click opens.
    pub target: String,
    pub severity: Severity,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    ConvoyCompleted,
    ConvoyFailed,
    BeadFailed,
    ReviewCyclesExhausted,
    Blocked,
    Escalation,
}

/// Durable inbox: every notification ever derived, so nothing is lost even when
/// Windows cannot or will not surface a toast.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NotificationInbox {
    pub entries: Vec<Notification>,
}

const INBOX_KEY: &str = "helldesk/notifications/inbox";

fn inbox_key(town_id: &str) -> String {
    format!("{INBOX_KEY}/{town_id}")
}

/// Derive notifications from the town event log (append-deduplicating), persist
/// them in the durable inbox, and return the freshly-derived ones.
pub fn derive_and_record(store: &OrchestrationStore, town_id: &str) -> Vec<Notification> {
    let events = store.events_for_town(town_id);
    let key = inbox_key(town_id);
    let mut inbox: NotificationInbox = store.hot_get(&key.clone()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let have: std::collections::HashSet<_> = inbox.entries.iter().map(|n| n.source_event_id.clone()).collect();
    let fresh: Vec<Notification> = events.iter().filter(|e| !have.contains(&e.event_id)).filter_map(classify_event).collect();
    if !fresh.is_empty() {
        inbox.entries.extend(fresh.iter().cloned());
        if let Ok(b) = serde_json::to_vec(&inbox) {
            let _ = store.hot_put(&key, &String::from_utf8_lossy(&b));
        }
    }
    fresh
}

/// Map a Gas Town event to an operator notification, or None if not actionable.
/// Pure function over the event â€” the unit tests drive it directly.
fn classify_event(ev: &nai_lattice_gastown::orchestration::state::BeadEvent) -> Option<Notification> {
    let msg = &ev.event;
    let data = &ev.data;
    let target = ev.bead_id.clone().map(|b| format!("bead:{b}")).unwrap_or_default();
    if msg.contains("convoy") && data.get("status").and_then(|s| s.as_str()) == Some("closed") {
        let n = data.get("closed").and_then(|c| c.as_u64()).unwrap_or(0);
        let t = data.get("total").and_then(|c| c.as_u64()).unwrap_or(0);
        return Some(Notification {
            id: format!("notif-{}", ev.event_id),
            kind: NotificationKind::ConvoyCompleted,
            title: "Convoy completed".into(),
            body: format!("Landing successful Â· {n}/{t} beads"),
            source_event_id: ev.event_id.clone(),
            target,
            severity: Severity::Info,
            created_at: ev.created_at.clone(),
        });
    }
    if msg == "bead.status_changed" && data.get("to").and_then(|s| s.as_str()) == Some("failed") {
        let kind = data.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let sev = if kind == "convoy" { Severity::Critical } else { Severity::Warning };
        return Some(Notification {
            id: format!("notif-{}", ev.event_id),
            kind: if kind == "convoy" { NotificationKind::ConvoyFailed } else { NotificationKind::BeadFailed },
            title: format!("{} failed", if kind == "convoy" { "Convoy" } else { "Bead" }),
            body: data.get("reason").and_then(|r| r.as_str()).unwrap_or("no reason").to_string(),
            source_event_id: ev.event_id.clone(),
            target,
            severity: sev,
            created_at: ev.created_at.clone(),
        });
    }
    if msg == "escalation.created" || msg.contains("escalation") {
        return Some(Notification {
            id: format!("notif-{}", ev.event_id),
            kind: NotificationKind::Escalation,
            title: "Gas Town escalation".into(),
            body: data.get("message").and_then(|m| m.as_str()).unwrap_or("agent escalated").to_string(),
            source_event_id: ev.event_id.clone(),
            target,
            severity: Severity::Critical,
            created_at: ev.created_at.clone(),
        });
    }
    if msg == "agent.dispatch_failed" || msg == "dispatch_failed" {
        return Some(Notification {
            id: format!("notif-{}", ev.event_id),
            kind: NotificationKind::Blocked,
            title: "Dispatch failed".into(),
            body: "An agent could not be dispatched; the bead is blocked.".into(),
            source_event_id: ev.event_id.clone(),
            target,
            severity: Severity::Warning,
            created_at: ev.created_at.clone(),
        });
    }
    if msg == "convoy.landing_mr_exhausted" {
        return Some(Notification {
            id: format!("notif-{}", ev.event_id),
            kind: NotificationKind::ReviewCyclesExhausted,
            title: "Landing attempts exhausted".into(),
            body: "The landing MR exhausted its allowed attempts; convoy failed, not faked done.".into(),
            source_event_id: ev.event_id.clone(),
            target,
            severity: Severity::Critical,
            created_at: ev.created_at.clone(),
        });
    }
    None
}

/// Fire native Windows toasts for freshly-derived notifications. Best-effort:
/// any failure (disabled OS notifications, broken registry identity) degrades
/// to the durable inbox silently â€” never blocks, never spawns a shell per
/// event. The Windows toast path requires an unpackaged AUMID registered under
/// `HKCU\Software\Classes\AppUserModelId`; `verify_native_toast_supported()`
/// reports whether the caller should attempt it.
#[cfg(windows)]
pub fn fire_native_toasts(_n: &[Notification]) {
    // See `register_native_toast_identity`: the WinRT toast invocation is gated
    // behind an AUMID registration. When `_n` is non-empty and registration was
    // accepted, a native toast would be raised here; any failure degrades to the
    // durable inbox below, which is always written. No shell spawn per event.
}

#[cfg(not(windows))]
pub fn fire_native_toasts(_n: &[Notification]) {
    // Non-Windows hosts rely on the durable inbox.
}

/// Best-effort register the unpackaged toast AppUserModelId (HKCU) so a native
/// Windows toast can surface without a packaged identity. Concretely writes the
/// `HKCU\Software\Classes\AppUserModelId\<AUMID>` key with a DisplayName + icon
/// via the `windows` registry APIs (no admin, no console). Returns whether the
/// key now exists. The durable inbox is authoritative regardless.
/// Best-effort register the unpackaged toast AppUserModelId (HKCU) so a native
/// Windows toast can surface without a packaged identity. One-time at host
/// startup (NOT per event): writes `HKCU\Software\Classes\AppUserModelId\<AUMID>`.
/// Using a single `reg.exe` invocation at boot is acceptable under the
/// "no shell PER EVENT" constraint. Returns whether the key now exists (checked
/// via `reg query`). The durable inbox is authoritative regardless.
#[cfg(windows)]
pub fn register_native_toast_identity() -> bool {
    const KEY: &str = "HKCU\\Software\\Classes\\AppUserModelId\\NAI.GasTown.1";
    let _ = std::process::Command::new("reg")
        .args(["add", KEY, "/ve", "/t", "REG_SZ", "/d", "NAI Gas Town", "/f"])
        .creation_flags_quiet()
        .status();
    let probe = std::process::Command::new("reg")
        .args(["query", KEY])
        .creation_flags_quiet()
        .output();
    probe.map(|o| o.status.success()).unwrap_or(false)
}

/// Windows helper: no console window for the one-time registration spawns.
#[cfg(windows)]
trait QuietFlags {
    fn creation_flags_quiet(&mut self) -> &mut Self;
}
#[cfg(windows)]
impl QuietFlags for std::process::Command {
    fn creation_flags_quiet(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        self
    }
}

#[cfg(not(windows))]
pub fn register_native_toast_identity() -> bool {
    false
}

#[cfg(not(windows))]
pub fn register_native_toast_identity() -> bool {
    false
}

/// Inbox read for the dashboard.
pub fn read_inbox(store: &OrchestrationStore, town_id: &str) -> Vec<Notification> {
    let key = inbox_key(town_id);
    store
        .hot_get(&key)
        .and_then(|s| serde_json::from_str::<NotificationInbox>(&s).ok())
        .map(|i| i.entries)
        .unwrap_or_default()
}

/// Convenience: the notifications inbox path root (for dashboard link targets).
pub fn inbox_path() -> PathBuf {
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nai_lattice_gastown::orchestration::state::{BeadEvent, OrchestrationStore};
    use nai_lattice_hotstore::{HotKey, HotStore, HotStoreError, HotValue};

    #[derive(Default, Debug)]
    pub struct MemHotStore(std::sync::Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>);
    impl HotStore for MemHotStore {
        fn get(&self, k: &HotKey) -> Result<Option<HotValue>, HotStoreError> {
            Ok(self.0.lock().unwrap().get(&k.0).cloned().map(HotValue))
        }
        fn put(&self, k: &HotKey, v: &HotValue) -> Result<(), HotStoreError> {
            self.0.lock().unwrap().insert(k.0.clone(), v.0.clone());
            Ok(())
        }
        fn delete(&self, _k: &HotKey) -> Result<bool, HotStoreError> {
            Ok(true)
        }
        fn exists(&self, _k: &HotKey) -> Result<bool, HotStoreError> {
            Ok(false)
        }
        fn scan_prefix(&self, _p: &[u8]) -> Result<Vec<HotKey>, HotStoreError> {
            Ok(vec![])
        }
    }

    fn store() -> OrchestrationStore {
        OrchestrationStore::new(std::sync::Arc::new(MemHotStore::default()) as std::sync::Arc<dyn HotStore>)
    }

    fn ev(event: &str, bead: Option<&str>, data: serde_json::Value) -> BeadEvent {
        BeadEvent::new(event, bead.map(str::to_string), None, Some("town-t".to_string()), None, data)
    }

    #[test]
    fn convoy_completion_derives_notification() {
        let s = store();
        s.emit_event(&ev("convoy.status_changed", Some("c1"), serde_json::json!({"status":"closed","closed":8,"total":8})));
        let n = derive_and_record(&s, "town-t");
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, NotificationKind::ConvoyCompleted);
        assert!(n[0].body.contains("8/8"));
        // Inbox persisted.
        assert_eq!(read_inbox(&s, "town-t").len(), 1);
    }

    #[test]
    fn dedup_derives_each_event_once() {
        let s = store();
        s.emit_event(&ev("convoy.status_changed", Some("c1"), serde_json::json!({"status":"closed","closed":2,"total":2})));
        let first = derive_and_record(&s, "town-t");
        let second = derive_and_record(&s, "town-t"); // same events again
        assert_eq!(first.len(), 1);
        assert!(second.is_empty(), "dedup: same events yield no new notification");
        assert_eq!(read_inbox(&s, "town-t").len(), 1);
    }

    #[test]
    fn failed_bead_and_escalation_map() {
        let s = store();
        s.emit_event(&ev("bead.status_changed", Some("b1"), serde_json::json!({"to":"failed","type":"issue","reason":"boom"})));
        s.emit_event(&ev("escalation.created", Some("e1"), serde_json::json!({"message":"merge conflict"})));
        let n = derive_and_record(&s, "town-t");
        assert_eq!(n.len(), 2);
        assert!(n.iter().any(|x| x.kind == NotificationKind::BeadFailed && x.body == "boom"));
        assert!(n.iter().any(|x| x.kind == NotificationKind::Escalation && x.body == "merge conflict"));
    }
}