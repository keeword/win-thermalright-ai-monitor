//! Identity and lifecycle facts. Logs never prove that a session is open.
use crate::agents::{AgentKind, Usage};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionKey {
    pub origin_id: String,
    pub agent_kind: AgentKind,
    pub native_session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn origin(id: &str) -> Origin {
        Origin {
            origin_id: id.into(),
            display_name: id.into(),
            ..Default::default()
        }
    }
    fn key(origin: &str, sid: &str) -> SessionKey {
        SessionKey {
            origin_id: origin.into(),
            agent_kind: AgentKind::Codex,
            native_session_id: sid.into(),
        }
    }
    fn instance(origin: &str, pid: u32, start: u64, sid: Option<&str>, now: i64) -> LiveInstance {
        LiveInstance {
            instance_key: InstanceKey {
                origin_id: origin.into(),
                boot_id: "boot".into(),
                pid,
                process_started_at: start,
            },
            agent_kind: AgentKind::Codex,
            session_key: sid.map(|s| key(origin, s)),
            shared_session_keys: vec![],
            native_work_state: None,
            last_verified_at: now,
            open_state: OpenState::Open,
            error: None,
        }
    }
    fn log(sid: &str) -> (AgentKind, Usage) {
        (
            AgentKind::Codex,
            Usage {
                session_id: sid.into(),
                project: "same-project".into(),
                waiting: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn shared_server_keeps_distinct_sessions_and_expires_all_associations_together() {
        let mut merger = Merger::default();
        let mut server = instance("wsl", 100, 123, None, 1000);
        server.shared_session_keys = vec![key("wsl", "a"), key("wsl", "b")];
        merger.update(
            origin("wsl"),
            vec![server.clone()],
            vec![log("a"), log("b")],
            true,
            1000,
        );
        assert_eq!(merger.snapshot.instances.len(), 1);
        assert_eq!(merger.snapshot.sessions.len(), 2);
        assert!(
            merger
                .snapshot
                .sessions
                .iter()
                .all(|s| s.open_state == OpenState::Open && s.instance_count == 1)
        );

        let unknown = instance("wsl", 100, 123, None, 1003);
        merger.update(origin("wsl"), vec![unknown], vec![], false, 1003);
        assert_eq!(merger.snapshot.sessions.len(), 2);
        assert!(
            merger
                .snapshot
                .sessions
                .iter()
                .all(|s| s.open_state == OpenState::Unconfirmed)
        );

        // A partial observation cannot close one of a shared server's threads.
        server.shared_session_keys.pop();
        server.last_verified_at = 1005;
        merger.update(origin("wsl"), vec![server.clone()], vec![], false, 1005);
        assert_eq!(merger.snapshot.sessions.len(), 2);
        assert!(
            merger
                .snapshot
                .sessions
                .iter()
                .all(|s| s.open_state == OpenState::Unconfirmed)
        );
        // A complete observation of one remaining thread removes the other.
        server.last_verified_at = 1006;
        merger.update(origin("wsl"), vec![server], vec![], true, 1006);
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert_eq!(merger.snapshot.sessions[0].key.native_session_id, "a");
        assert_eq!(merger.snapshot.sessions[0].open_state, OpenState::Open);
        merger.refresh(1013);
        assert_eq!(
            merger.snapshot.sessions[0].open_state,
            OpenState::Unconfirmed
        );
        merger.update(origin("wsl"), vec![], vec![], true, 1014);
        merger.update(origin("wsl"), vec![], vec![], true, 1017);
        assert!(merger.snapshot.sessions.is_empty());
    }
    fn snapshot(count: usize) -> AgentSnapshot {
        AgentSnapshot {
            sessions: (0..count)
                .map(|n| AgentSession {
                    key: key("windows", &format!("id-{n:03}")),
                    usage: log("x").1,
                    open_state: OpenState::Open,
                    work_state: WorkState::Idle,
                    evidence: "test".into(),
                    first_seen: n as u64,
                    last_verified_at: None,
                    instance_count: 1,
                })
                .collect(),
            ..Default::default()
        }
    }
    #[test]
    fn distinct_sessions_and_three_origins_merge_without_project_identity() {
        let mut merger = Merger::default();
        for name in ["windows", "ubuntu", "debian"] {
            merger.update(
                origin(name),
                vec![
                    instance(name, 1, 1, Some("one"), 100),
                    instance(name, 2, 1, Some("two"), 100),
                ],
                vec![log("one"), log("two")],
                true,
                100,
            );
        }
        assert_eq!(merger.snapshot.sessions.len(), 6);
        assert!(
            merger
                .snapshot
                .sessions
                .iter()
                .all(|s| s.open_state == OpenState::Open && s.work_state == WorkState::Idle)
        );
    }
    #[test]
    fn history_alone_is_never_open_and_unlinked_instance_never_borrows_history() {
        let mut merger = Merger::default();
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, None, 100)],
            vec![log("history")],
            true,
            100,
        );
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert!(
            merger.snapshot.sessions[0]
                .key
                .native_session_id
                .starts_with("unlinked-")
        );
        assert_eq!(
            merger.snapshot.sessions[0].open_state,
            OpenState::Unconfirmed
        );
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("real"), 101)],
            vec![log("real")],
            true,
            101,
        );
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert_eq!(merger.snapshot.sessions[0].key.native_session_id, "real");
    }

    #[test]
    fn association_failure_keeps_known_content_unconfirmed_and_verified_pid_reuse_closes_it() {
        let mut merger = Merger::default();
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("known"), 100)],
            vec![log("known")],
            true,
            100,
        );
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, None, 101)],
            vec![],
            false,
            101,
        );
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert_eq!(merger.snapshot.sessions[0].key.native_session_id, "known");
        assert_eq!(
            merger.snapshot.sessions[0].open_state,
            OpenState::Unconfirmed
        );
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 2, None, 102)],
            vec![],
            false,
            102,
        );
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert!(
            merger.snapshot.sessions[0]
                .key
                .native_session_id
                .starts_with("unlinked-")
        );
    }

    #[test]
    fn identified_session_is_visible_while_its_log_backfills() {
        let mut merger = Merger::default();
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("native-id"), 100)],
            vec![],
            true,
            100,
        );
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert_eq!(
            merger.snapshot.sessions[0].key.native_session_id,
            "native-id"
        );
        assert_eq!(merger.snapshot.sessions[0].open_state, OpenState::Open);
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Unknown);
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("native-id"), 101)],
            vec![log("native-id")],
            true,
            101,
        );
        assert_eq!(merger.snapshot.sessions[0].usage.project, "same-project");
    }
    #[test]
    fn two_instances_keep_one_card_and_two_omissions_confirm_exit() {
        let mut merger = Merger::default();
        merger.update(
            origin("windows"),
            vec![
                instance("windows", 1, 1, Some("one"), 100),
                instance("windows", 2, 1, Some("one"), 100),
            ],
            vec![log("one")],
            true,
            100,
        );
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert_eq!(merger.snapshot.sessions[0].instance_count, 2);
        for now in [101, 102] {
            merger.update(
                origin("windows"),
                vec![instance("windows", 2, 1, Some("one"), now)],
                vec![],
                true,
                now,
            );
        }
        assert_eq!(merger.snapshot.sessions[0].instance_count, 1);
        merger.update(origin("windows"), vec![], vec![], true, 103);
        assert_eq!(merger.snapshot.sessions.len(), 1);
        merger.update(origin("windows"), vec![], vec![], true, 104);
        assert!(merger.snapshot.sessions.is_empty());
    }
    #[test]
    fn failure_and_partial_snapshot_expire_to_unconfirmed_and_recover() {
        let mut merger = Merger::default();
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("one"), 100)],
            vec![log("one")],
            true,
            100,
        );
        for now in [105, 108, 110] {
            merger.update(origin("windows"), vec![], vec![], false, now);
        }
        assert_eq!(
            merger.snapshot.sessions[0].open_state,
            OpenState::Unconfirmed
        );
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("one"), 111)],
            vec![],
            true,
            111,
        );
        assert_eq!(merger.snapshot.sessions[0].open_state, OpenState::Open);
    }
    #[test]
    fn pid_reuse_and_session_switch_do_not_inherit_association() {
        let mut merger = Merger::default();
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("old"), 100)],
            vec![log("old")],
            true,
            100,
        );
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("new"), 101)],
            vec![log("new")],
            true,
            101,
        );
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert_eq!(merger.snapshot.sessions[0].key.native_session_id, "new");
        for now in [102, 103] {
            merger.update(
                origin("windows"),
                vec![instance("windows", 1, 2, None, now)],
                vec![],
                true,
                now,
            );
        }
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert_eq!(
            merger.snapshot.sessions[0].open_state,
            OpenState::Unconfirmed
        );
    }
    #[test]
    fn live_old_log_stays_open_but_silent_work_is_unknown() {
        let mut merger = Merger::default();
        let mut usage = log("old").1;
        usage.working = true;
        usage.waiting = false;
        usage.last_activity = chrono::DateTime::from_timestamp(1, 0);
        merger.update(
            origin("windows"),
            vec![instance("windows", 1, 1, Some("old"), 10000)],
            vec![(AgentKind::Codex, usage)],
            true,
            10000,
        );
        assert_eq!(merger.snapshot.sessions[0].open_state, OpenState::Open);
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Unknown);
    }
    #[test]
    fn details_traverse_every_session_once_with_single_last_column() {
        for count in [0, 1, 2, 3, 6, 7, 9] {
            let snapshot = snapshot(count);
            let now = Instant::now();
            let mut view = ViewState::default();
            let preferences = ViewPreferences {
                mode: ViewMode::Details,
                ..Default::default()
            };
            view.sync(&snapshot, &preferences, now, false);
            assert_eq!(view.pages(), count.div_ceil(2).max(1));
            let mut visited = vec![];
            for _ in 0..view.pages() {
                visited.extend(view.range().map(|i| view.keys[i].clone()));
                view.advance(1, now);
            }
            assert_eq!(visited.len(), count);
            assert_eq!(
                visited
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                count
            );
            assert_eq!(view.page, 0);
            view.sync(&snapshot, &ViewPreferences::default(), now, false);
            assert_eq!(
                view.mode,
                if count > 2 {
                    ViewMode::Overview
                } else {
                    ViewMode::Details
                }
            );
        }
    }
    #[test]
    fn click_selects_detail_page_and_pager_pauses_rotation() {
        let mut view = ViewState::default();
        let now = Instant::now();
        let prefs = ViewPreferences {
            mode: ViewMode::Overview,
            auto_rotate: true,
            ..Default::default()
        };
        view.sync(&snapshot(9), &prefs, now, false);
        assert!(view.click(1200.0, 260.0, now)); // global index 5, detail page 2
        assert_eq!(view.page, 2);
        assert_eq!(view.mode, ViewMode::Details);
        assert!(!view.preferences.auto_rotate);
        assert!(view.click(420.0, 390.0, now));
        assert_eq!(view.page, 0);
    }
    #[test]
    fn rotation_pauses_during_blank_and_does_not_catch_up_after_wake() {
        let mut view = ViewState::default();
        let now = Instant::now();
        let data = snapshot(9);
        let prefs = ViewPreferences {
            mode: ViewMode::Overview,
            auto_rotate: true,
            ..Default::default()
        };
        view.sync(&data, &prefs, now, false);
        view.sync(&data, &prefs, now + Duration::from_secs(8), false);
        assert_eq!(view.page, 1);
        view.sync(&data, &prefs, now + Duration::from_secs(100), true);
        view.sync(&data, &prefs, now + Duration::from_secs(200), false);
        assert_eq!(view.page, 1);
        view.sync(&data, &prefs, now + Duration::from_secs(207), false);
        assert_eq!(view.page, 1);
        view.sync(&data, &prefs, now + Duration::from_secs(208), false);
        assert_eq!(view.page, 0);
    }
    #[test]
    fn filters_keep_mode_and_usage_and_anchor_survives_sorting() {
        let mut view = ViewState::default();
        let now = Instant::now();
        let mut data = snapshot(9);
        data.daily_usage.input = 100;
        let prefs = ViewPreferences {
            mode: ViewMode::Details,
            ..Default::default()
        };
        view.sync(&data, &prefs, now, false);
        view.advance(2, now);
        let anchor = view.keys[4].clone();
        data.sessions[8].work_state = WorkState::Blocked;
        view.sync(&data, &prefs, now, false);
        assert!(view.range().any(|i| view.keys[i] == anchor));
        view.sync(
            &data,
            &ViewPreferences {
                agent_filter: "claude".into(),
                ..prefs
            },
            now,
            false,
        );
        assert!(view.keys.is_empty());
        assert_eq!(view.page, 0);
        assert_eq!(view.mode, ViewMode::Details);
        assert_eq!(data.daily_usage.input, 100);
    }
    #[test]
    fn native_claude_status_survives_silent_work_but_not_stale_process_evidence() {
        let mut merger = Merger::default();
        let mut live = instance("windows", 1, 2, Some("current"), 1000);
        live.agent_kind = AgentKind::Claude;
        live.session_key.as_mut().unwrap().agent_kind = AgentKind::Claude;
        live.native_work_state = Some(WorkState::Working);
        let mut usage = log("current").1;
        usage.last_activity = chrono::DateTime::from_timestamp(100, 0);
        merger.update(
            origin("windows"),
            vec![live.clone()],
            vec![(AgentKind::Claude, usage)],
            true,
            1000,
        );
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Working);
        live.native_work_state = Some(WorkState::Idle);
        live.last_verified_at = 1003;
        merger.update(origin("windows"), vec![live], vec![], true, 1003);
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Idle);
        merger.refresh(1010);
        assert_eq!(
            merger.snapshot.sessions[0].open_state,
            OpenState::Unconfirmed
        );
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Unknown);
    }

    #[test]
    fn native_claude_status_is_scoped_to_the_current_session_and_combines_instances() {
        let mut merger = Merger::default();
        let mut live = instance("windows", 1, 2, Some("old"), 100);
        live.agent_kind = AgentKind::Claude;
        live.session_key.as_mut().unwrap().agent_kind = AgentKind::Claude;
        live.native_work_state = Some(WorkState::Working);
        merger.update(origin("windows"), vec![live.clone()], vec![], true, 100);
        live.session_key.as_mut().unwrap().native_session_id = "new".into();
        live.native_work_state = Some(WorkState::Idle);
        merger.update(origin("windows"), vec![live.clone()], vec![], true, 100);
        assert_eq!(merger.snapshot.sessions.len(), 1);
        assert_eq!(merger.snapshot.sessions[0].key.native_session_id, "new");
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Idle);
        let mut second = live.clone();
        second.instance_key.pid = 3;
        second.native_work_state = Some(WorkState::Working);
        merger.update(origin("windows"), vec![live, second], vec![], true, 100);
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Working);
        assert_eq!(merger.snapshot.sessions[0].instance_count, 2);
    }

    #[test]
    fn native_claude_status_preserves_explicit_attention_and_falls_back_to_logs() {
        let mut merger = Merger::default();
        let mut live = instance("windows", 1, 2, Some("current"), 100);
        live.agent_kind = AgentKind::Claude;
        live.session_key.as_mut().unwrap().agent_kind = AgentKind::Claude;
        live.native_work_state = Some(WorkState::Working);
        let mut usage = log("current").1;
        usage.blocked = true;
        merger.update(
            origin("windows"),
            vec![live.clone()],
            vec![(AgentKind::Claude, usage)],
            true,
            100,
        );
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Blocked);
        live.native_work_state = Some(WorkState::Idle);
        merger.update(origin("windows"), vec![live.clone()], vec![], true, 100);
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Idle);
        live.native_work_state = None;
        merger.update(
            origin("windows"),
            vec![live],
            vec![(AgentKind::Claude, log("current").1)],
            true,
            100,
        );
        assert_eq!(merger.snapshot.sessions[0].work_state, WorkState::Idle);
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceKey {
    pub origin_id: String,
    pub boot_id: String,
    pub pid: u32,
    pub process_started_at: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OpenState {
    Open,
    Closed,
    #[default]
    Unconfirmed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WorkState {
    Working,
    Blocked,
    Idle,
    #[default]
    Unknown,
}
impl WorkState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "工作中",
            Self::Blocked => "待处理",
            Self::Idle => "闲置",
            Self::Unknown => "工作状态未知",
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Origin {
    pub origin_id: String,
    pub display_name: String,
    pub platform: String,
    pub distro_name: Option<String>,
    pub user: Option<String>,
    pub status: String,
    pub last_success: Option<i64>,
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct LiveInstance {
    pub instance_key: InstanceKey,
    pub agent_kind: AgentKind,
    pub session_key: Option<SessionKey>,
    /// Explicitly identified sessions owned by a shared Codex app-server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_session_keys: Vec<SessionKey>,
    /// Status from a Claude registration whose process identity was verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_work_state: Option<WorkState>,
    pub last_verified_at: i64,
    pub open_state: OpenState,
    #[serde(default)]
    pub error: Option<String>,
}
impl LiveInstance {
    pub fn session_keys(&self) -> impl Iterator<Item = &SessionKey> {
        self.session_key.iter().chain(&self.shared_session_keys)
    }
    pub fn is_linked(&self) -> bool {
        self.session_keys().next().is_some()
    }
    pub fn has_session(&self, key: &SessionKey) -> bool {
        self.session_keys().any(|s| s == key)
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub key: SessionKey,
    pub usage: Usage,
    pub open_state: OpenState,
    pub work_state: WorkState,
    pub evidence: String,
    pub first_seen: u64,
    pub last_verified_at: Option<i64>,
    pub instance_count: usize,
}
impl AgentSession {
    pub fn label(&self) -> &'static str {
        if self.open_state == OpenState::Unconfirmed {
            "待确认"
        } else {
            self.work_state.label()
        }
    }
    pub fn priority(&self) -> u8 {
        if self.open_state != OpenState::Open {
            4
        } else {
            match self.work_state {
                WorkState::Blocked => 0,
                WorkState::Working => 1,
                WorkState::Idle => 2,
                WorkState::Unknown => 3,
            }
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct DailyUsage {
    pub available: bool,
    pub day: String,
    pub input: u64,
    pub output: u64,
    pub coverage: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub sessions: Vec<AgentSession>,
    pub instances: Vec<LiveInstance>,
    pub origins: Vec<Origin>,
    pub daily_usage: DailyUsage,
    pub origins_ready: bool,
}
impl AgentSnapshot {
    pub fn short_id(&self, key: &SessionKey) -> String {
        let id = &key.native_session_id;
        let mut n = 4;
        while self.sessions.iter().any(|s| {
            s.key != *key
                && s.key
                    .native_session_id
                    .chars()
                    .take(n)
                    .eq(id.chars().take(n))
        }) && n < id.chars().count()
        {
            n += 1;
        }
        id.chars().take(n).collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    Auto,
    Overview,
    Details,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewPreferences {
    pub mode: ViewMode,
    pub agent_filter: String,
    pub origin_filter: String,
    pub auto_rotate: bool,
    pub rotate_interval_seconds: u64,
}
impl Default for ViewPreferences {
    fn default() -> Self {
        Self {
            mode: ViewMode::Auto,
            agent_filter: "all".into(),
            origin_filter: "all".into(),
            auto_rotate: false,
            rotate_interval_seconds: 8,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ViewState {
    pub preferences: ViewPreferences,
    pub mode: ViewMode,
    pub page: usize,
    pub keys: Vec<SessionKey>,
    #[serde(skip, default = "Instant::now")]
    next_rotate: Instant,
    paused: bool,
}
impl Default for ViewState {
    fn default() -> Self {
        Self {
            preferences: ViewPreferences::default(),
            mode: ViewMode::Details,
            page: 0,
            keys: vec![],
            next_rotate: Instant::now(),
            paused: false,
        }
    }
}
impl ViewState {
    pub fn page_size(&self) -> usize {
        if self.mode == ViewMode::Overview {
            6
        } else {
            2
        }
    }
    pub fn pages(&self) -> usize {
        self.keys.len().div_ceil(self.page_size()).max(1)
    }
    pub fn range(&self) -> std::ops::Range<usize> {
        let start = self.page * self.page_size();
        start..(start + self.page_size()).min(self.keys.len())
    }
    pub fn sync(
        &mut self,
        snapshot: &AgentSnapshot,
        preferences: &ViewPreferences,
        now: Instant,
        blank: bool,
    ) {
        let changed = self.preferences != *preferences;
        let anchor = self.keys.get(self.page * self.page_size()).cloned();
        self.preferences = preferences.clone();
        self.mode = match preferences.mode {
            ViewMode::Auto => {
                if snapshot
                    .sessions
                    .iter()
                    .filter(|s| s.open_state == OpenState::Open)
                    .count()
                    > 2
                {
                    ViewMode::Overview
                } else {
                    ViewMode::Details
                }
            }
            other => other,
        };
        let mut sessions: Vec<_> = snapshot
            .sessions
            .iter()
            .filter(|s| {
                s.open_state != OpenState::Closed
                    && (preferences.agent_filter == "all"
                        || s.key
                            .agent_kind
                            .name()
                            .eq_ignore_ascii_case(&preferences.agent_filter))
                    && (preferences.origin_filter == "all"
                        || s.key.origin_id == preferences.origin_filter)
            })
            .collect();
        sessions.sort_by(|a, b| {
            (a.priority(), a.first_seen, &a.key).cmp(&(b.priority(), b.first_seen, &b.key))
        });
        self.keys = sessions.iter().map(|s| s.key.clone()).collect();
        if changed {
            self.page = 0;
            self.reset_timer(now);
        } else if let Some(index) = anchor.and_then(|key| self.keys.iter().position(|k| *k == key))
        {
            self.page = index / self.page_size();
        }
        self.page = self.page.min(self.pages() - 1);
        if blank || self.paused != blank {
            self.reset_timer(now);
        }
        self.paused = blank;
        if !blank && preferences.auto_rotate && self.pages() > 1 && now >= self.next_rotate {
            self.advance(1, now);
        }
    }
    pub fn reset_timer(&mut self, now: Instant) {
        self.next_rotate =
            now + Duration::from_secs(self.preferences.rotate_interval_seconds.clamp(1, 86400));
    }
    pub fn advance(&mut self, delta: isize, now: Instant) {
        self.page = (self.page as isize + delta).rem_euclid(self.pages() as isize) as usize;
        self.reset_timer(now);
    }
    pub fn click(&mut self, x: f32, y: f32, now: Instant) -> bool {
        if !(404.0..1538.0).contains(&x) {
            return false;
        }
        if (378.0..406.0).contains(&y) && self.pages() > 1 {
            self.page = (((x - 416.0).max(0.0) / 1110.0) * self.pages() as f32) as usize;
            self.page = self.page.min(self.pages() - 1);
            self.preferences.auto_rotate = false;
            self.reset_timer(now);
            return true;
        }
        if self.mode == ViewMode::Overview
            && (416.0..1526.0).contains(&x)
            && (86.0..376.0).contains(&y)
        {
            let col = ((x - 416.0).max(0.0) / 374.0) as usize;
            let row = ((y - 86.0) / 145.0) as usize;
            let index = self.page * 6 + row * 3 + col;
            if col < 3
                && (x - 416.0) % 374.0 < 362.0
                && (y - 86.0) % 145.0 < 133.0
                && index < self.keys.len()
            {
                self.mode = ViewMode::Details;
                self.preferences.mode = ViewMode::Details;
                self.preferences.auto_rotate = false;
                self.page = index / 2;
                self.reset_timer(now);
                return true;
            }
        }
        false
    }
}

/// A successful partial observation cannot remove instances. Two complete
/// omissions close an instance; stale observations instead become unconfirmed.
#[derive(Default)]
pub struct Merger {
    pub snapshot: AgentSnapshot,
    misses: HashMap<InstanceKey, u8>,
    next_seen: u64,
}
impl Merger {
    pub fn update(
        &mut self,
        origin: Origin,
        instances: Vec<LiveInstance>,
        logs: Vec<(AgentKind, Usage)>,
        complete: bool,
        now: i64,
    ) {
        let id = &origin.origin_id;
        let observed: HashMap<_, _> = instances
            .iter()
            .map(|i| (i.instance_key.clone(), i))
            .collect();
        for old in self
            .snapshot
            .instances
            .iter_mut()
            .filter(|i| i.instance_key.origin_id == *id)
        {
            if let Some(new) = observed.get(&old.instance_key) {
                let previous_session = old.session_key.clone();
                let previous_shared = old.shared_session_keys.clone();
                *old = (*new).clone();
                // Losing a file/registration is not proof that this same
                // instance left its previously identified conversation.
                if !old.is_linked() && (previous_session.is_some() || !previous_shared.is_empty()) {
                    old.session_key = previous_session;
                    old.shared_session_keys = previous_shared;
                    old.open_state = OpenState::Unconfirmed;
                } else if !complete {
                    for key in previous_shared {
                        if !old.has_session(&key) {
                            old.shared_session_keys.push(key);
                            old.open_state = OpenState::Unconfirmed;
                        }
                    }
                }
                self.misses.remove(&old.instance_key);
            } else if instances.iter().any(|new| {
                new.instance_key.pid == old.instance_key.pid
                    && new.open_state == OpenState::Open
                    && new.instance_key != old.instance_key
            }) {
                // A verified replacement with this PID proves the old process
                // ended, even when another part of the snapshot is incomplete.
                old.open_state = OpenState::Closed;
            } else if complete {
                let misses = self.misses.entry(old.instance_key.clone()).or_default();
                *misses += 1;
                if *misses >= 2 {
                    old.open_state = OpenState::Closed;
                }
            }
        }
        for instance in instances {
            if !self
                .snapshot
                .instances
                .iter()
                .any(|i| i.instance_key == instance.instance_key)
            {
                self.snapshot.instances.push(instance);
            }
        }
        self.snapshot
            .instances
            .retain(|i| i.open_state != OpenState::Closed);
        for (kind, usage) in logs {
            let key = SessionKey {
                origin_id: id.clone(),
                agent_kind: kind,
                native_session_id: usage.session_id.clone(),
            };
            if let Some(session) = self.snapshot.sessions.iter_mut().find(|s| s.key == key) {
                session.usage = usage;
            } else {
                self.next_seen += 1;
                self.snapshot.sessions.push(AgentSession {
                    key,
                    usage,
                    open_state: OpenState::Closed,
                    work_state: WorkState::Unknown,
                    evidence: "日志推断".into(),
                    first_seen: self.next_seen,
                    last_verified_at: None,
                    instance_count: 0,
                });
            }
        }
        for instance in &self.snapshot.instances {
            for key in instance.session_keys().filter(|key| key.origin_id == *id) {
                if !self.snapshot.sessions.iter().any(|s| s.key == *key) {
                    self.next_seen += 1;
                    self.snapshot.sessions.push(AgentSession {
                        key: key.clone(),
                        usage: Usage {
                            session_id: key.native_session_id.clone(),
                            message: "日志尚未读取或正在补齐".into(),
                            ..Default::default()
                        },
                        open_state: instance.open_state,
                        work_state: WorkState::Unknown,
                        evidence: "进程关联".into(),
                        first_seen: self.next_seen,
                        last_verified_at: Some(instance.last_verified_at),
                        instance_count: 1,
                    });
                }
            }
        }
        for instance in self
            .snapshot
            .instances
            .iter()
            .filter(|i| i.instance_key.origin_id == *id && !i.is_linked())
        {
            let key = SessionKey {
                origin_id: id.clone(),
                agent_kind: instance.agent_kind,
                native_session_id: unlinked_id(&instance.instance_key),
            };
            if !self.snapshot.sessions.iter().any(|s| s.key == key) {
                self.next_seen += 1;
                self.snapshot.sessions.push(AgentSession {
                    key,
                    usage: Usage {
                        project: "未关联实例".into(),
                        message: instance
                            .error
                            .clone()
                            .unwrap_or_else(|| "尚未取得真实会话身份".into()),
                        ..Default::default()
                    },
                    open_state: OpenState::Unconfirmed,
                    work_state: WorkState::Unknown,
                    evidence: "进程".into(),
                    first_seen: self.next_seen,
                    last_verified_at: Some(now),
                    instance_count: 1,
                });
            }
        }
        if let Some(old) = self
            .snapshot
            .origins
            .iter_mut()
            .find(|o| o.origin_id == *id)
        {
            *old = origin;
        } else {
            self.snapshot.origins.push(origin);
        }
        self.refresh(now);
    }
    pub fn stop_origin(&mut self, id: &str, now: i64) {
        self.snapshot
            .instances
            .retain(|i| i.instance_key.origin_id != id);
        if let Some(origin) = self.snapshot.origins.iter_mut().find(|o| o.origin_id == id) {
            origin.status = "已停止".into();
        }
        self.refresh(now);
    }
    pub fn refresh(&mut self, now: i64) {
        for instance in &mut self.snapshot.instances {
            if now - instance.last_verified_at > 6 {
                instance.open_state = OpenState::Unconfirmed;
            }
        }
        for session in &mut self.snapshot.sessions {
            let instances: Vec<_> = self
                .snapshot
                .instances
                .iter()
                .filter(|i| {
                    i.has_session(&session.key)
                        || (!i.is_linked()
                            && session.key.origin_id == i.instance_key.origin_id
                            && session.key.native_session_id == unlinked_id(&i.instance_key))
                })
                .collect();
            session.instance_count = instances.len();
            if instances
                .iter()
                .any(|i| i.shared_session_keys.contains(&session.key))
            {
                session.evidence = "共享 Codex app-server · 日志句柄".into();
            }
            session.last_verified_at = instances
                .iter()
                .map(|i| i.last_verified_at)
                .max()
                .or(session.last_verified_at);
            session.open_state = if instances
                .iter()
                .any(|i| i.open_state == OpenState::Open && i.is_linked())
            {
                OpenState::Open
            } else if !instances.is_empty() {
                OpenState::Unconfirmed
            } else {
                OpenState::Closed
            };
            session.usage.age = session
                .usage
                .last_activity
                .map(|at| (now - at.timestamp()).max(0) as u64)
                .unwrap_or(u64::MAX);
            session.work_state = if session.open_state != OpenState::Open {
                WorkState::Unknown
            } else if session.usage.blocked {
                WorkState::Blocked
            } else if session.usage.waiting {
                WorkState::Idle
            } else if session.usage.working && session.usage.age < 90 {
                WorkState::Working
            } else {
                WorkState::Unknown
            };
            if session.open_state == OpenState::Open && session.key.agent_kind == AgentKind::Claude
            {
                session.evidence = "Claude 原生会话登记 · 进程身份".into();
            }
            if session.open_state == OpenState::Open
                && let Some(native_state) = instances
                    .iter()
                    .filter(|i| {
                        i.open_state == OpenState::Open
                            && i.agent_kind == AgentKind::Claude
                            && i.has_session(&session.key)
                    })
                    .filter_map(|i| i.native_work_state)
                    .min_by_key(|state| match state {
                        WorkState::Blocked => 0,
                        WorkState::Working => 1,
                        WorkState::Idle => 2,
                        WorkState::Unknown => 3,
                    })
            {
                session.work_state = if native_state == WorkState::Working && session.usage.blocked
                {
                    WorkState::Blocked
                } else {
                    native_state
                };
                session.evidence = "Claude 原生会话登记 · 原生状态".into();
            }
        }
        // Keep history in the log collectors; the public snapshot contains cards only.
        self.snapshot
            .sessions
            .retain(|s| s.open_state != OpenState::Closed);
    }
}

fn unlinked_id(key: &InstanceKey) -> String {
    format!(
        "unlinked-{}-{}-{}",
        key.pid, key.process_started_at, key.boot_id
    )
}
