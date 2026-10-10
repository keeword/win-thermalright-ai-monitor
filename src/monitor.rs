//! Two fair, independent source workers publish complete immutable snapshots.
use crate::{
    agents::{AgentKind, Collector, LogSnapshot},
    config::AgentSettings,
    probe::{self, ProbeResult},
    session::{AgentSnapshot, Merger, Origin},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicBool, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Job {
    id: String,
    distro: Option<String>,
    registration: String,
    user: Option<String>,
}
struct Work {
    job: Job,
    result: anyhow::Result<(ProbeResult, LogSnapshot)>,
}
pub struct Monitor {
    settings: AgentSettings,
    jobs: Vec<Job>,
    cursor: usize,
    inflight: HashSet<String>,
    due: HashMap<String, Instant>,
    origin_ids: HashMap<String, String>,
    discovery: Instant,
    send: Option<mpsc::Sender<Job>>,
    receive: mpsc::Receiver<Work>,
    workers: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    merger: Merger,
    ledger: HashMap<(AgentKind, String), (u64, u64)>,
    day: chrono::NaiveDate,
    coverage: HashMap<String, bool>,
}
impl Monitor {
    pub fn new(home: PathBuf, settings: AgentSettings) -> Self {
        let (send, queue) = mpsc::channel::<Job>();
        let queue = Arc::new(Mutex::new(queue));
        let (results, receive) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let collectors = Arc::new(Mutex::new(HashMap::<String, Collector>::new()));
        let cursors = Arc::new(Mutex::new(HashMap::<String, u64>::new()));
        let mut workers = vec![];
        for _ in 0..2 {
            let queue = queue.clone();
            let results = results.clone();
            let stop = stop.clone();
            let home = home.clone();
            let collectors = collectors.clone();
            let cursors = cursors.clone();
            workers.push(thread::spawn(move || {
                loop {
                    let next = { queue.lock().unwrap().recv() };
                    let Ok(job) = next else {
                        break;
                    };
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let mut collector = collectors
                        .lock()
                        .unwrap()
                        .remove(&job.id)
                        .unwrap_or_else(|| Collector::new(home.clone()));
                    let cursor = *cursors.lock().unwrap().entry(job.id.clone()).or_default();
                    let result = (|| {
                        let facts = if let Some(distro) = &job.distro {
                            probe::guest(
                                distro,
                                job.user.as_deref(),
                                &job.registration,
                                &collector.offsets(),
                                cursor,
                                &stop,
                            )?
                        } else {
                            serde_json::from_slice(&probe::local(&stop)?)?
                        };
                        *cursors.lock().unwrap().entry(job.id.clone()).or_default() += 3;
                        let mut logs = if job.distro.is_some() {
                            collector.ingest(&facts.files)
                        } else {
                            collector.collect_logs(&facts.logs)
                        };
                        logs.backfilling |= facts.backfilling;
                        Ok((facts, logs))
                    })();
                    collectors.lock().unwrap().insert(job.id.clone(), collector);
                    if results.send(Work { job, result }).is_err() {
                        break;
                    }
                }
            }));
        }
        Self {
            settings,
            jobs: vec![],
            cursor: 0,
            inflight: HashSet::new(),
            due: HashMap::new(),
            origin_ids: HashMap::new(),
            discovery: Instant::now(),
            send: Some(send),
            receive,
            workers,
            stop,
            merger: Merger::default(),
            ledger: HashMap::new(),
            day: chrono::Local::now().date_naive(),
            coverage: HashMap::new(),
        }
    }
    pub fn settings(&mut self, settings: AgentSettings) {
        self.settings = settings;
    }
    fn discover(&mut self, stop: &AtomicBool) {
        let mut jobs = vec![];
        if self.settings.windows_enabled {
            jobs.push(Job {
                id: "windows".into(),
                distro: None,
                registration: String::new(),
                user: None,
            });
        }
        let mut success = true;
        if self.settings.wsl_running_enabled {
            let discovery = probe::registrations().and_then(|registrations| {
                if registrations.is_empty() {
                    Ok(vec![])
                } else {
                    let running = probe::running(stop)?;
                    Ok(registrations
                        .into_iter()
                        .filter(|(_, name)| running.contains(name))
                        .collect())
                }
            });
            match discovery {
                Ok(distros) => {
                    for (registration, distro) in distros {
                        if self.settings.wsl_default_user {
                            jobs.push(Job {
                                id: format!("wsl:{registration}:default"),
                                distro: Some(distro.clone()),
                                registration: registration.clone(),
                                user: None,
                            });
                        }
                        for extra in self
                            .settings
                            .extra_wsl_users
                            .iter()
                            .filter(|u| u.distro == distro)
                        {
                            jobs.push(Job {
                                id: format!("wsl:{registration}:{}", extra.user),
                                distro: Some(distro.clone()),
                                registration: registration.clone(),
                                user: Some(extra.user.clone()),
                            });
                        }
                    }
                }
                Err(error) => {
                    success = false;
                    jobs.extend(self.jobs.iter().filter(|j| j.distro.is_some()).cloned());
                    self.merger.update(
                        Origin {
                            origin_id: "wsl-discovery".into(),
                            display_name: "WSL 来源发现".into(),
                            status: "发现失败".into(),
                            error: Some(error.to_string()),
                            ..Default::default()
                        },
                        vec![],
                        vec![],
                        false,
                        chrono::Utc::now().timestamp(),
                    );
                }
            }
        }
        if success {
            self.merger
                .snapshot
                .origins
                .retain(|o| o.origin_id != "wsl-discovery");
        }
        for previous in &self.jobs {
            if !jobs.iter().any(|j| j.id == previous.id)
                && let Some(id) = self.origin_ids.get(&previous.id)
                && !jobs.iter().any(|j| self.origin_ids.get(&j.id) == Some(id))
            {
                self.merger.stop_origin(id, chrono::Utc::now().timestamp());
            }
        }
        self.jobs = jobs;
        self.discovery = Instant::now() + Duration::from_secs(5);
    }
    pub fn tick(&mut self, stop: &AtomicBool) -> AgentSnapshot {
        if Instant::now() >= self.discovery {
            self.discover(stop);
        }
        let day = chrono::Local::now().date_naive();
        if self.day != day {
            self.ledger.clear();
            self.coverage.clear();
            self.day = day;
        }
        while let Ok(work) = self.receive.try_recv() {
            self.inflight.remove(&work.job.id);
            if !self.jobs.iter().any(|j| j.id == work.job.id) {
                continue;
            }
            let now = chrono::Utc::now().timestamp();
            let mut origin = Origin {
                origin_id: self
                    .origin_ids
                    .get(&work.job.id)
                    .cloned()
                    .unwrap_or_else(|| work.job.id.clone()),
                display_name: work.job.distro.clone().unwrap_or_else(|| "Windows".into()),
                platform: if work.job.distro.is_some() {
                    "wsl2"
                } else {
                    "windows"
                }
                .into(),
                distro_name: work.job.distro.clone(),
                user: work.job.user.clone(),
                ..Default::default()
            };
            match work.result {
                Ok((facts, logs)) => {
                    if origin.origin_id != facts.origin_id {
                        self.merger
                            .snapshot
                            .origins
                            .retain(|o| o.origin_id != work.job.id);
                    }
                    origin.origin_id = facts.origin_id.clone();
                    origin.user = Some(facts.user.clone());
                    if work.job.user.is_some() {
                        origin.display_name = format!("{} / {}", origin.display_name, facts.user);
                    }
                    self.origin_ids
                        .insert(work.job.id.clone(), origin.origin_id.clone());
                    let errors: Vec<_> = facts
                        .errors
                        .iter()
                        .chain(logs.errors.iter())
                        .cloned()
                        .collect();
                    let refill = logs.backfilling && errors.is_empty();
                    origin.status = if !errors.is_empty() {
                        "采集不完整"
                    } else if logs.backfilling {
                        "用量正在补齐"
                    } else if facts.instances.is_empty() {
                        "无可监控会话"
                    } else if facts.instances.iter().any(|i| !i.is_linked()) {
                        "部分实例待关联"
                    } else {
                        "采集正常"
                    }
                    .into();
                    origin.last_success = Some(now);
                    origin.error = if errors.is_empty() {
                        None
                    } else {
                        Some(errors.join("; "))
                    };
                    self.coverage
                        .insert(work.job.id.clone(), !logs.backfilling && errors.is_empty());
                    for (kind, id, tokens) in logs
                        .events
                        .into_iter()
                        .filter(|_| logs.day == Some(self.day))
                    {
                        let value = self.ledger.entry((kind, id)).or_default();
                        value.0 = value.0.max(tokens.0);
                        value.1 = value.1.max(tokens.1);
                    }
                    self.merger
                        .update(origin, facts.instances, logs.sessions, facts.complete, now);
                    self.due.insert(
                        work.job.id,
                        Instant::now()
                            + if refill {
                                Duration::from_millis(500)
                            } else {
                                Duration::from_secs(3)
                            },
                    );
                }
                Err(error) => {
                    self.coverage.insert(work.job.id.clone(), false);
                    origin.status = "探测失败".into();
                    origin.error = Some(error.to_string());
                    origin.last_success = self
                        .merger
                        .snapshot
                        .origins
                        .iter()
                        .find(|o| o.origin_id == origin.origin_id)
                        .and_then(|o| o.last_success);
                    self.merger.update(origin, vec![], vec![], false, now);
                    self.due
                        .insert(work.job.id, Instant::now() + Duration::from_secs(6));
                }
            }
        }
        // Round-robin, at most two probes including queued work. Never enqueue
        // a second probe for the same source.
        for _ in 0..self.jobs.len() {
            if self.inflight.len() >= 2 {
                break;
            }
            self.cursor %= self.jobs.len();
            let job = self.jobs[self.cursor].clone();
            self.cursor += 1;
            let alias = self.origin_ids.get(&job.id).is_some_and(|id| {
                self.jobs
                    .iter()
                    .take(self.cursor - 1)
                    .any(|other| self.origin_ids.get(&other.id) == Some(id))
            });
            if alias
                || self.inflight.contains(&job.id)
                || self
                    .due
                    .get(&job.id)
                    .is_some_and(|due| *due > Instant::now())
            {
                continue;
            }
            if self.send.as_ref().unwrap().send(job.clone()).is_ok() {
                self.inflight.insert(job.id);
            }
        }
        self.merger.refresh(chrono::Utc::now().timestamp());
        self.merger.snapshot.daily_usage = crate::session::DailyUsage {
            available: !self.ledger.is_empty()
                || (self
                    .jobs
                    .iter()
                    .all(|j| self.coverage.get(&j.id).copied().unwrap_or(false))
                    && !self
                        .merger
                        .snapshot
                        .origins
                        .iter()
                        .any(|o| o.error.is_some())),
            day: self.day.to_string(),
            input: self.ledger.values().map(|v| v.0).sum(),
            output: self.ledger.values().map(|v| v.1).sum(),
            coverage: if self
                .jobs
                .iter()
                .any(|j| !self.coverage.get(&j.id).copied().unwrap_or(false))
                || self
                    .merger
                    .snapshot
                    .origins
                    .iter()
                    .any(|o| o.error.is_some())
            {
                "部分来源 / 正在补齐"
            } else {
                "已读取来源 · 日志读数"
            }
            .into(),
        };
        self.merger.snapshot.origins_ready = self
            .jobs
            .iter()
            .all(|j| self.origin_ids.contains_key(&j.id))
            && !self
                .merger
                .snapshot
                .origins
                .iter()
                .any(|o| o.origin_id == "wsl-discovery");
        self.merger.snapshot.clone()
    }
    pub fn collect_once(&mut self, stop: &AtomicBool) -> AgentSnapshot {
        self.tick(stop);
        let deadline =
            Instant::now() + Duration::from_secs((self.jobs.len() as u64).div_ceil(2) * 5 + 1);
        while Instant::now() < deadline && self.coverage.len() < self.jobs.len() {
            thread::sleep(Duration::from_millis(40));
            self.tick(stop);
        }
        self.tick(stop)
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.send.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_usage_deduplicates_events_across_sources_and_resets_at_midnight() {
        let home = tempfile::tempdir().unwrap();
        let mut monitor = Monitor::new(home.path().into(), AgentSettings::default());
        let future = Instant::now() + Duration::from_secs(60);
        monitor.discovery = future;
        let (send, receive) = mpsc::channel();
        monitor.receive = receive;
        let day = monitor.day;
        for id in ["windows", "wsl"] {
            let job = Job {
                id: id.into(),
                distro: None,
                registration: String::new(),
                user: None,
            };
            monitor.jobs.push(job.clone());
            monitor.due.insert(job.id.clone(), future);
            send.send(Work {
                job,
                result: Ok((
                    ProbeResult {
                        origin_id: id.into(),
                        complete: true,
                        ..Default::default()
                    },
                    LogSnapshot {
                        day: Some(day),
                        events: vec![
                            (AgentKind::Claude, "forked-message".into(), (10, 2)),
                            (AgentKind::Claude, "forked-message".into(), (20, 3)),
                            (AgentKind::Codex, "guardian-usage".into(), (5, 1)),
                        ],
                        ..Default::default()
                    },
                )),
            })
            .unwrap();
        }
        let stop = AtomicBool::new(false);
        let usage = monitor.tick(&stop).daily_usage;
        assert!(usage.available);
        assert_eq!((usage.input, usage.output), (25, 4));
        let unchanged = monitor.tick(&stop).daily_usage;
        assert_eq!((unchanged.input, unchanged.output), (25, 4));

        monitor.day = day.pred_opt().unwrap();
        send.send(Work {
            job: monitor.jobs[0].clone(),
            result: Ok((
                ProbeResult {
                    origin_id: "windows".into(),
                    complete: true,
                    ..Default::default()
                },
                LogSnapshot {
                    day: Some(monitor.day),
                    events: vec![(AgentKind::Claude, "yesterday".into(), (100, 50))],
                    ..Default::default()
                },
            )),
        })
        .unwrap();
        let usage = monitor.tick(&stop).daily_usage;
        assert_eq!(usage.day, day.to_string());
        assert_eq!((usage.input, usage.output), (0, 0));
    }
}
