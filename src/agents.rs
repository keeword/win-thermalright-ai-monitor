use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AgentKind {
    Claude,
    Codex,
    Cursor,
}
impl AgentKind {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Cursor];
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    pub current: usize,
    pub total: usize,
    pub completed: usize,
    pub text: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub available: bool,
    pub project: String,
    pub message: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub working: bool,
    pub attention: bool,
    pub waiting: bool,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub project_path: String,
    pub age: u64,
    pub plan: Option<Plan>,
    pub quota_used: Option<f64>,
    pub quota_reset: Option<i64>,
    pub context_used: Option<f64>,
    pub session_id: String,
    pub log_path: Option<PathBuf>,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct Session {
    offset: u64,
    modified: Option<SystemTime>,
    usage: Usage,
    session_id: String,
    cumulative: (u64, u64),
    tokens: HashMap<String, (u64, u64)>,
    quota_time: String,
    attention_since: Option<SystemTime>,
    subagent: bool,
    kind: Option<AgentKind>,
    file_id: String,
    parse_error: Option<String>,
    last_event: Option<SystemTime>,
    recent_usage: Option<Usage>,
    recent_event: Option<SystemTime>,
}

struct FileStamp {
    size: u64,
    modified: SystemTime,
    activity: SystemTime,
}

pub struct Collector {
    home: PathBuf,
    day: NaiveDate,
    sessions: HashMap<PathBuf, Session>,
    stamps: HashMap<PathBuf, FileStamp>,
}
impl Collector {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            day: Local::now().date_naive(),
            sessions: HashMap::new(),
            stamps: HashMap::new(),
        }
    }
    pub fn collect_logs(&mut self, required: &[(AgentKind, PathBuf)]) -> LogSnapshot {
        let required: Vec<_> = required
            .iter()
            .map(|(kind, path)| (*kind, path.canonicalize().unwrap_or_else(|_| path.clone())))
            .collect();
        let day = Local::now().date_naive();
        if self.day != day {
            self.sessions.clear();
            self.day = day;
        }
        let mut result = LogSnapshot {
            day: Some(self.day),
            ..Default::default()
        };
        let mut budget = 16 * 1024 * 1024;
        let mut present = HashSet::new();
        let mut discovered = HashSet::new();
        for kind in AgentKind::ALL {
            let root = if kind == AgentKind::Codex {
                std::env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .map(|p| p.join("sessions"))
            } else if kind == AgentKind::Claude {
                std::env::var_os("CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from)
                    .map(|p| p.join("projects"))
            } else {
                None
            }
            .unwrap_or_else(|| {
                self.home.join(match kind {
                    AgentKind::Claude => ".claude/projects",
                    AgentKind::Codex => ".codex/sessions",
                    AgentKind::Cursor => ".cursor/projects",
                })
            });
            let mut latest: Option<(SystemTime, Usage, String)> = None;
            let mut daily: HashMap<String, (u64, u64)> = HashMap::new();
            let mut quota: Option<(String, Option<f64>, Option<i64>)> = None;
            let mut files = Vec::new();
            let root = root.canonicalize().unwrap_or(root);
            let mut roots = vec![root.clone()];
            if matches!(kind, AgentKind::Codex | AgentKind::Claude) {
                for (_, path) in required.iter().filter(|(k, _)| *k == kind) {
                    if let Some(log_root) = path.ancestors().find(|p| {
                        p.file_name().is_some_and(|n| {
                            n == if kind == AgentKind::Claude {
                                "projects"
                            } else {
                                "sessions"
                            }
                        })
                    }) {
                        let log_root = log_root
                            .canonicalize()
                            .unwrap_or_else(|_| log_root.to_path_buf());
                        if !roots.contains(&log_root) {
                            roots.push(log_root);
                        }
                    }
                }
            }
            for scan_root in roots {
                for entry in walkdir::WalkDir::new(&scan_root).follow_links(false) {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            if error
                                .io_error()
                                .is_none_or(|e| e.kind() != std::io::ErrorKind::NotFound)
                            {
                                result.errors.push(error.to_string());
                            }
                            continue;
                        }
                    };
                    let path = entry.path();
                    if !entry.file_type().is_file() || path.extension().is_none_or(|e| e != "jsonl")
                    {
                        continue;
                    }
                    let relative = path.strip_prefix(&scan_root).unwrap_or(path);
                    if kind == AgentKind::Claude && relative.components().count() != 2 {
                        continue;
                    }
                    if kind == AgentKind::Cursor
                        && !relative
                            .components()
                            .any(|c| c.as_os_str() == "agent-transcripts")
                    {
                        continue;
                    }
                    let metadata = match entry.metadata() {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            result.errors.push(error.to_string());
                            continue;
                        }
                    };
                    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    discovered.insert(path.to_path_buf());
                    let activity = if kind == AgentKind::Codex {
                        let stamp = self.stamps.entry(path.to_path_buf()).or_insert(FileStamp {
                            size: u64::MAX,
                            modified,
                            activity: modified,
                        });
                        // NTFS may leave LastWriteTime unchanged until the writer closes
                        // its handle. Size detects appends; embedded timestamps select
                        // the real latest session, including one spanning midnight.
                        if stamp.size != metadata.len() || stamp.modified != modified {
                            stamp.activity = last_record_time(path).unwrap_or(modified);
                            stamp.size = metadata.len();
                            stamp.modified = modified;
                        }
                        stamp.activity
                    } else {
                        modified
                    };
                    files.push((path.to_path_buf(), metadata.len(), modified, activity));
                }
            }
            for (_, path) in required.iter().filter(|(k, _)| *k == kind) {
                if !files.iter().any(|(p, _, _, _)| p == path)
                    && let Ok(meta) = std::fs::metadata(path)
                {
                    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    files.push((path.clone(), meta.len(), modified, modified));
                }
            }
            files.sort_unstable_by_key(|(path, _, _, activity)| {
                (
                    !required.iter().any(|(_, p)| p == path),
                    std::cmp::Reverse(*activity),
                )
            });
            // Read today's active logs plus recent fallback sessions. Activity
            // comes from the transcript on Codex, not NTFS write timestamps.
            for (index, (path, size, modified, activity)) in files.into_iter().enumerate() {
                if index >= 8
                    && DateTime::<Local>::from(activity).date_naive() < self.day
                    && !required.iter().any(|(_, p)| p == &path)
                {
                    continue;
                }
                let path = path.as_path();
                let relative = path.strip_prefix(&root).unwrap_or(path);
                present.insert(path.to_path_buf());
                let session = self.sessions.entry(path.to_path_buf()).or_default();
                if size < session.offset
                    || (size == session.offset && session.modified.is_some_and(|t| t != modified))
                {
                    *session = Session::default();
                }
                let file_id = local_file_id(path);
                if !session.file_id.is_empty() && session.file_id != file_id {
                    *session = Session::default();
                }
                session.file_id = file_id;
                session.kind = Some(kind);
                session.modified = Some(modified);
                let before = session.offset;
                if size > session.offset
                    && budget > 0
                    && let Err(error) = read_new_limited(path, session, kind, self.day, budget)
                {
                    result.errors.push(format!("{}: {error}", path.display()));
                }
                budget = budget.saturating_sub((session.offset - before) as usize);
                if let Some(error) = &session.parse_error {
                    result.errors.push(format!("{}: {error}", path.display()));
                }
                if session.offset < size {
                    result.backfilling = true;
                }
                for (id, tokens) in &session.tokens {
                    result.events.push((kind, id.clone(), *tokens));
                    let target = daily.entry(id.clone()).or_default();
                    target.0 = target.0.max(tokens.0);
                    target.1 = target.1.max(tokens.1);
                }
                if session.usage.quota_used.is_some()
                    && quota.as_ref().is_none_or(|q| session.quota_time > q.0)
                {
                    quota = Some((
                        session.quota_time.clone(),
                        session.usage.quota_used,
                        session.usage.quota_reset,
                    ));
                }
                // Internal review/spawned agents are counted for usage, but must
                // never replace the project/message shown for a user's main agent.
                if kind == AgentKind::Codex && session.subagent {
                    continue;
                }
                // Ignore sessions containing no main-chain activity (e.g. side chains).
                if session.usage.project.is_empty()
                    && session.usage.message.is_empty()
                    && !session.usage.working
                    && !session.usage.waiting
                {
                    continue;
                }
                let active_at = session.last_event.unwrap_or(activity);
                {
                    let mut usage = session.usage.clone();
                    usage.age = active_at.elapsed().unwrap_or_default().as_secs();
                    usage.working = usage.working && usage.age < 90;
                    usage.attention = usage.waiting
                        && usage.age < 900
                        && session.attention_since.is_some_and(|t| {
                            t.elapsed().unwrap_or_default() < Duration::from_secs(10)
                        });
                    let sid = if session.session_id.is_empty() {
                        path.file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        session.session_id.clone()
                    };
                    usage.session_id = sid.clone();
                    usage.log_path = Some(path.to_path_buf());
                    usage.last_activity = Some(DateTime::<Utc>::from(active_at));
                    if usage.project.is_empty() {
                        usage.project = relative
                            .components()
                            .next()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned())
                            .unwrap_or_default();
                    }
                    usage.available = true;
                    usage.input = session.tokens.values().map(|v| v.0).sum();
                    usage.output = session.tokens.values().map(|v| v.1).sum();
                    if kind == AgentKind::Cursor {
                        cursor_details(&self.home, &sid, &mut usage);
                    }
                    result.sessions.push((kind, usage.clone()));
                    if latest.as_ref().is_none_or(|v| active_at > v.0) {
                        latest = Some((active_at, usage, sid));
                    }
                }
            }
            let (mut usage, sid) = latest.map(|(_, u, s)| (u, s)).unwrap_or_default();
            usage.input = 0;
            usage.output = 0;
            usage.available = root.exists();
            for (input, output) in daily.values() {
                usage.input = usage.input.saturating_add(*input);
                usage.output = usage.output.saturating_add(*output);
            }
            if let Some((_, used, reset)) = quota {
                usage.quota_used = used;
                usage.quota_reset = reset;
            }
            if kind == AgentKind::Cursor {
                cursor_details(&self.home, &sid, &mut usage);
            }
            result.legacy.insert(kind, usage);
        }
        self.sessions.retain(|p, _| present.contains(p));
        self.stamps.retain(|p, _| discovered.contains(p));
        result
    }
    #[cfg(test)]
    pub fn collect(&mut self) -> HashMap<AgentKind, Usage> {
        self.collect_logs(&[]).legacy
    }

    pub fn offsets(&mut self) -> HashMap<String, (String, u64)> {
        let day = Local::now().date_naive();
        if self.day != day {
            self.sessions.clear();
            self.day = day;
        }
        self.sessions
            .iter()
            .map(|(p, s)| {
                (
                    p.to_string_lossy().into_owned(),
                    (s.file_id.clone(), s.offset),
                )
            })
            .collect()
    }
    pub fn ingest(&mut self, files: &[crate::probe::RemoteFile]) -> LogSnapshot {
        let day = Local::now().date_naive();
        if self.day != day {
            self.sessions.clear();
            self.day = day;
        }
        let mut result = LogSnapshot {
            day: Some(self.day),
            ..Default::default()
        };
        for file in files {
            if file
                .next_offset
                .is_some_and(|next| next < file.offset || next > file.size)
            {
                result
                    .errors
                    .push(format!("{}: invalid physical log cursor", file.path));
                continue;
            }
            let path = PathBuf::from(&file.path);
            let session = self.sessions.entry(path.clone()).or_default();
            if session.file_id != file.file_id || file.offset == 0 && session.offset != 0 {
                *session = Session::default();
            }
            if file.offset != session.offset {
                result
                    .errors
                    .push(format!("{}: log cursor mismatch", file.path));
                continue;
            }
            session.file_id = file.file_id.clone();
            session.kind = Some(file.kind);
            for line in file.data.split_inclusive('\n') {
                if !line.ends_with('\n') {
                    break;
                }
                session.offset += line.len() as u64;
                match serde_json::from_str(line) {
                    Ok(value) => apply(session, file.kind, &value, self.day),
                    Err(error) => session.parse_error = Some(error.to_string()),
                }
            }
            if let Some(next) = file.next_offset {
                session.offset = next;
            }
            if !file.recent_data.is_empty() {
                let mut recent = Session::default();
                for line in file.recent_data.lines() {
                    match serde_json::from_str(line) {
                        Ok(value) => apply(&mut recent, file.kind, &value, self.day),
                        Err(error) => result
                            .errors
                            .push(format!("{}: recent log: {error}", file.path)),
                    }
                }
                if recent.last_event.is_some() {
                    if recent.usage.model.is_empty() {
                        recent.usage.model = session.usage.model.clone();
                    }
                    if session.session_id.is_empty() {
                        session.session_id = recent.session_id.clone();
                    }
                    session.recent_event = recent.last_event;
                    session.recent_usage = Some(recent.usage);
                }
            }
            if session.offset < file.size {
                result.backfilling = true;
            }
        }
        for (path, session) in &self.sessions {
            let Some(kind) = session.kind else {
                continue;
            };
            if let Some(error) = &session.parse_error {
                result.errors.push(format!("{}: {error}", path.display()));
            }
            for (id, tokens) in &session.tokens {
                result.events.push((kind, id.clone(), *tokens));
            }
            if session.subagent {
                continue;
            }
            let mut usage = if session.recent_event >= session.last_event {
                session
                    .recent_usage
                    .as_ref()
                    .unwrap_or(&session.usage)
                    .clone()
            } else {
                session.usage.clone()
            };
            usage.session_id = if session.session_id.is_empty() {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            } else {
                session.session_id.clone()
            };
            usage.log_path = Some(path.clone());
            usage.last_activity = session
                .last_event
                .max(session.recent_event)
                .map(DateTime::<Utc>::from);
            usage.available = true;
            usage.input = session.tokens.values().map(|v| v.0).sum();
            usage.output = session.tokens.values().map(|v| v.1).sum();
            result.sessions.push((kind, usage));
        }
        result
    }
}

#[derive(Default)]
pub struct LogSnapshot {
    pub day: Option<NaiveDate>,
    pub sessions: Vec<(AgentKind, Usage)>,
    pub events: Vec<(AgentKind, String, (u64, u64))>,
    pub backfilling: bool,
    pub errors: Vec<String>,
    legacy: HashMap<AgentKind, Usage>,
}

fn local_file_id(path: &Path) -> String {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        if let Ok(file) = File::open(path) {
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0 {
                return format!(
                    "{}:{}:{}",
                    info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
                );
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            return format!("{}:{}", metadata.dev(), metadata.ino());
        }
    }
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.created().ok())
        .map(|t| format!("{t:?}"))
        .unwrap_or_default()
}

/// Read the latest complete record timestamp without parsing historical bodies.
/// Metadata is cached so this tail read happens only when a file changes.
fn last_record_time(path: &Path) -> Option<SystemTime> {
    use std::io::Read;
    #[derive(Deserialize)]
    struct Timestamp {
        timestamp: Option<DateTime<Utc>>,
    }
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let start = size.saturating_sub(1024 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let end = bytes.iter().rposition(|b| *b == b'\n')?;
    let begin = if start == 0 {
        0
    } else {
        bytes.iter().position(|b| *b == b'\n')? + 1
    };
    if begin > end {
        return None;
    }
    bytes[begin..=end]
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<Timestamp>(line).ok()?.timestamp)
        .next()
        .map(SystemTime::from)
}

#[cfg(test)]
fn read_new(
    path: &Path,
    session: &mut Session,
    kind: AgentKind,
    day: NaiveDate,
) -> std::io::Result<()> {
    read_new_limited(path, session, kind, day, 16 * 1024 * 1024)
}
fn read_new_limited(
    path: &Path,
    session: &mut Session,
    kind: AgentKind,
    day: NaiveDate,
    limit: usize,
) -> std::io::Result<()> {
    use std::io::Read;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(session.offset))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    // Limit one pass so a large initial transcript cannot pin the collector forever.
    let mut consumed = 0;
    while consumed < limit {
        line.clear();
        let n = (&mut reader)
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut line)?;
        if n > 1024 * 1024 {
            session.parse_error = Some("JSONL record exceeds 1 MiB".into());
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "JSONL record exceeds 1 MiB",
            ));
        }
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        session.offset += n as u64;
        consumed += n;
        match serde_json::from_slice::<Value>(&line) {
            Ok(value) => apply(session, kind, &value, day),
            Err(error) => session.parse_error = Some(error.to_string()),
        }
    }
    Ok(())
}
fn string(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
fn num(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}
fn text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return clean(s);
    }
    clean(
        &content
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|v| ["text", "output_text"].contains(&string(&v["type"])))
                    .map(|v| string(&v["text"]))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
    )
}
fn clean(s: &str) -> String {
    s.trim().chars().take(1600).collect()
}
fn project(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(cwd)
        .to_owned()
}
fn state(session: &mut Session, waiting: bool, timestamp: &str) {
    if waiting && !session.usage.waiting {
        // Old completed sessions should not flash every time the application starts.
        session.attention_since = DateTime::parse_from_rfc3339(timestamp)
            .ok()
            .map(SystemTime::from)
            .or(session.modified)
            .or(Some(SystemTime::now()));
    }
    if !waiting {
        session.attention_since = None;
    }
    session.usage.blocked = false;
    session.usage.waiting = waiting;
    session.usage.working = !waiting;
}
fn plan(items: &Value) -> Option<Plan> {
    let items = items.as_array()?;
    if items.is_empty() {
        return None;
    }
    let completed = items
        .iter()
        .filter(|i| string(&i["status"]) == "completed")
        .count();
    if completed == items.len() {
        return None;
    }
    let index = items
        .iter()
        .position(|i| string(&i["status"]) == "in_progress")
        .or_else(|| {
            items
                .iter()
                .position(|i| string(&i["status"]) != "completed")
        })?;
    let item = &items[index];
    let label = ["activeForm", "step", "content"]
        .iter()
        .map(|k| string(&item[k]))
        .find(|s| !s.is_empty())
        .unwrap_or("");
    Some(Plan {
        current: index + 1,
        total: items.len(),
        completed,
        text: clean(label),
    })
}

fn apply(session: &mut Session, kind: AgentKind, v: &Value, day: NaiveDate) {
    if v["isSidechain"].as_bool() == Some(true) {
        return;
    }
    if let Some(id) = v["sessionId"].as_str() {
        session.session_id = id.to_owned();
    }
    let timestamp = string(&v["timestamp"]);
    let record_time = DateTime::parse_from_rfc3339(timestamp).ok();
    let today = record_time.is_some_and(|t| t.with_timezone(&Local).date_naive() == day);
    if let Some(at) = record_time.map(SystemTime::from) {
        session.last_event = Some(session.last_event.map_or(at, |previous| previous.max(at)));
    }
    if !string(&v["cwd"]).is_empty() {
        session.usage.project_path = string(&v["cwd"]).to_owned();
        session.usage.project = project(string(&v["cwd"]));
    }
    if kind == AgentKind::Codex {
        let p = &v["payload"];
        let ty = string(&p["type"]);
        if string(&v["type"]) == "session_meta" {
            session.session_id = string(&p["id"]).to_owned();
            session.usage.project_path = string(&p["cwd"]).to_owned();
            session.usage.project = project(string(&p["cwd"]));
            session.subagent =
                p["source"].get("subagent").is_some() || string(&p["source"]) == "subagent";
        }
        if string(&v["type"]) == "turn_context" {
            session.usage.model = string(&p["model"]).to_owned();
            if !string(&p["cwd"]).is_empty() {
                session.usage.project = project(string(&p["cwd"]));
            }
        }
        if ty == "token_count" {
            let info = &p["info"];
            let total = &info["total_token_usage"];
            let current = (
                num(&total["input_tokens"]) + num(&total["cache_write_input_tokens"]),
                num(&total["output_tokens"]),
            );
            let delta = if total.is_object() {
                let d = (
                    current.0.saturating_sub(session.cumulative.0),
                    current.1.saturating_sub(session.cumulative.1),
                );
                session.cumulative = current;
                d
            } else {
                let last = &info["last_token_usage"];
                (
                    num(&last["input_tokens"]) + num(&last["cache_write_input_tokens"]),
                    num(&last["output_tokens"]),
                )
            };
            if today && delta != (0, 0) {
                let id = format!(
                    "{}:{}:{}:{}",
                    session.session_id, timestamp, current.0, current.1
                );
                session.tokens.insert(id, delta);
            }
            let quota = &p["rate_limits"]["primary"];
            if let Some(used) = quota["used_percent"].as_f64() {
                session.usage.quota_used = Some(used);
                session.usage.quota_reset = quota["resets_at"].as_i64();
                session.quota_time = timestamp.to_owned();
            }
        }
        match ty {
            "task_started" | "user_message" => {
                session.usage.plan = None;
                state(session, false, timestamp);
            }
            "task_complete" | "turn_aborted" => {
                session.usage.plan = None;
                state(session, true, timestamp);
                let message = string(&p["last_agent_message"]);
                if !message.is_empty() {
                    session.usage.message = clean(message);
                }
            }
            "agent_message" => {
                session.usage.message = clean(string(&p["message"]));
            }
            "function_call" | "custom_tool_call" => {
                state(session, false, timestamp);
                if string(&p["name"]).ends_with("update_plan")
                    && let Ok(args) = serde_json::from_str::<Value>(string(&p["arguments"]))
                {
                    session.usage.plan = plan(&args["plan"]);
                }
            }
            "function_call_output"
            | "custom_tool_call_output"
            | "agent_reasoning"
            | "reasoning" => state(session, false, timestamp),
            "message" => {
                if string(&p["role"]) == "user" {
                    session.usage.plan = None;
                    state(session, false, timestamp);
                }
                if string(&p["role"]) == "assistant" {
                    let message = text(&p["content"]);
                    if !message.is_empty() {
                        session.usage.message = message;
                    }
                    if string(&p["phase"]) == "final_answer" {
                        state(session, true, timestamp);
                        session.usage.plan = None;
                    }
                }
            }
            _ if ty.contains("approval_request") => {
                state(session, true, timestamp);
                session.usage.blocked = true;
            }
            _ => {}
        }
    } else {
        let role = if kind == AgentKind::Claude {
            string(&v["type"])
        } else {
            string(&v["role"])
        };
        if string(&v["type"]) == "turn_ended" {
            state(session, true, timestamp);
            session.usage.plan = None;
        }
        let msg = &v["message"];
        if role == "user" {
            let real = msg["content"].is_string()
                || msg["content"]
                    .as_array()
                    .is_some_and(|b| b.iter().any(|x| string(&x["type"]) != "tool_result"));
            if real {
                session.usage.plan = None;
            }
            state(session, false, timestamp);
        }
        if role == "assistant" {
            let model = string(&msg["model"]);
            if !model.is_empty() {
                session.usage.model = model.to_owned();
            }
            let message = text(&msg["content"]);
            if !message.is_empty() {
                session.usage.message = message;
            }
            let mut tool = false;
            let mut asks_user = false;
            if let Some(blocks) = msg["content"].as_array() {
                for block in blocks {
                    if string(&block["type"]) == "tool_use" {
                        tool = true;
                        asks_user |= string(&block["name"]) == "AskUserQuestion";
                        if string(&block["name"]) == "TodoWrite" {
                            session.usage.plan = plan(&block["input"]["todos"]);
                        }
                    }
                }
            }
            let waiting = if kind == AgentKind::Claude {
                match string(&msg["stop_reason"]) {
                    "end_turn" | "stop_sequence" | "max_tokens" => true,
                    "tool_use" => false,
                    _ => !tool,
                }
            } else {
                !tool
            };
            state(session, waiting, timestamp);
            if kind == AgentKind::Claude && !waiting && asks_user {
                session.usage.blocked = true;
            }
            if waiting {
                session.usage.plan = None;
            }
            if kind == AgentKind::Claude && today {
                let usage = &msg["usage"];
                let id = string(&msg["id"]);
                if !id.is_empty() && usage.is_object() {
                    let input = num(&usage["input_tokens"])
                        + num(&usage["cache_creation_input_tokens"])
                        + num(&usage["cache_read_input_tokens"]);
                    let output = num(&usage["output_tokens"]);
                    let entry = session.tokens.entry(id.to_owned()).or_default();
                    entry.0 = entry.0.max(input);
                    entry.1 = entry.1.max(output);
                }
            }
        }
    }
}

fn cursor_details(home: &Path, sid: &str, usage: &mut Usage) {
    use rusqlite::{Connection, OpenFlags};
    let appdata = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Roaming"));
    let path = appdata.join("Cursor/User/globalStorage/state.vscdb");
    if let Ok(db) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        let _ = db.busy_timeout(Duration::from_millis(50));
        if let Ok(mut query) =
            db.prepare("SELECT value FROM composerHeaders WHERE composerId = ?1 LIMIT 1")
            && let Ok(value) = query.query_row([sid], |r| r.get::<_, String>(0))
            && let Ok(v) = serde_json::from_str::<Value>(&value)
        {
            usage.context_used = v["contextUsagePercent"].as_f64();
        }
    }
    if usage.model.is_empty()
        && let Ok(db) = Connection::open_with_flags(
            home.join(".cursor/ai-tracking/ai-code-tracking.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    {
        let _ = db.busy_timeout(Duration::from_millis(50));
        if let Ok(model)=db.query_row("SELECT model FROM ai_code_hashes WHERE conversationId = ?1 AND model IS NOT NULL ORDER BY timestamp DESC LIMIT 1",[sid],|r| r.get::<_,String>(0)) {usage.model=model;}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_stop_reasons_and_explicit_questions_determine_log_state() {
        let mut session = Session::default();
        let day = Local::now().date_naive();
        apply(
            &mut session,
            AgentKind::Claude,
            &serde_json::json!({
                "type":"assistant", "message":{"stop_reason":"tool_use","content":[{"type":"text","text":"continuing"}]}
            }),
            day,
        );
        assert!(session.usage.working);
        apply(
            &mut session,
            AgentKind::Claude,
            &serde_json::json!({
                "type":"assistant", "message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"AskUserQuestion"}]}
            }),
            day,
        );
        assert!(session.usage.blocked);
        apply(
            &mut session,
            AgentKind::Claude,
            &serde_json::json!({
                "type":"user", "message":{"content":[{"type":"tool_result","content":"answered"}]}
            }),
            day,
        );
        assert!(!session.usage.blocked);
        assert!(session.usage.working);
        apply(
            &mut session,
            AgentKind::Claude,
            &serde_json::json!({
                "type":"assistant", "message":{"stop_reason":"end_turn","content":[{"type":"tool_use","name":"AskUserQuestion"}]}
            }),
            day,
        );
        assert!(session.usage.waiting);
        assert!(!session.usage.blocked);
    }

    #[test]
    fn native_claude_custom_root_includes_closed_sessions_in_daily_usage() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("other-account/projects/project");
        std::fs::create_dir_all(&root).unwrap();
        for (sid, tokens) in [("live", 10), ("closed", 20)] {
            let record = serde_json::json!({"sessionId":sid,"timestamp":Local::now().to_rfc3339(),
                "type":"assistant","message":{"id":sid,"stop_reason":"end_turn","content":[],
                    "usage":{"input_tokens":tokens,"output_tokens":1}}});
            std::fs::write(root.join(format!("{sid}.jsonl")), format!("{record}\n")).unwrap();
        }
        let mut collector = Collector::new(home.path().to_path_buf());
        let snapshot = collector.collect_logs(&[(AgentKind::Claude, root.join("live.jsonl"))]);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|(kind, _, _)| *kind == AgentKind::Claude)
                .map(|(_, _, (input, _))| *input)
                .sum::<u64>(),
            30
        );
    }
    use super::*;
    use serde_json::json;
    fn write_codex_log(
        path: &Path,
        id: &str,
        cwd: &str,
        source: Value,
        at: DateTime<Utc>,
        message: &str,
    ) {
        let lines = [
            json!({"timestamp":at,"type":"session_meta","payload":{"id":id,"cwd":cwd,"source":source}}),
            json!({"timestamp":at,"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":message}]}}),
            json!({"timestamp":at,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":20}}}}),
        ];
        let data = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn codex_selects_latest_events_despite_stale_windows_mtime() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".codex/sessions");
        std::fs::create_dir_all(&root).unwrap();
        let now = Utc::now();
        // More than eight recently touched older conversations must not push
        // an active, still-open Windows log out of discovery or selection.
        for n in 0..10 {
            write_codex_log(
                &root.join(format!("old-{n}.jsonl")),
                &format!("old-{n}"),
                "E:\\old-project",
                json!("vscode"),
                now - chrono::Duration::hours(1),
                "old message",
            );
        }
        let active = root.join("active.jsonl");
        write_codex_log(
            &active,
            "active",
            "E:\\Source\\current-project",
            json!("vscode"),
            now,
            "current assistant message",
        );
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&active)
            .unwrap()
            .set_modified(SystemTime::from(now - chrono::Duration::days(2)))
            .unwrap();
        let mut collector = Collector::new(home.path().into());
        let usage = collector.collect().remove(&AgentKind::Codex).unwrap();
        assert_eq!(usage.session_id, "active");
        assert_eq!(usage.project, "current-project");
        assert_eq!(usage.message, "current assistant message");
        assert!(usage.age < 10);
        assert_eq!(usage.input, 1100);
        // Detect another append while LastWriteTime stays unchanged.
        use std::io::Write;
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&active)
            .unwrap();
        writeln!(writer,"{}",json!({"timestamp":now+chrono::Duration::seconds(1),"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"new message"}]}})).unwrap();
        writer
            .set_modified(SystemTime::from(now - chrono::Duration::days(2)))
            .unwrap();
        assert_eq!(
            collector.collect()[&AgentKind::Codex].message,
            "new message"
        );
    }

    #[test]
    fn codex_guardian_does_not_replace_main_conversation() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".codex/sessions");
        std::fs::create_dir_all(&root).unwrap();
        let now = Utc::now();
        write_codex_log(
            &root.join("main.jsonl"),
            "main",
            "E:\\my-project",
            json!("vscode"),
            now - chrono::Duration::seconds(5),
            "message to the user",
        );
        write_codex_log(
            &root.join("guardian.jsonl"),
            "guardian",
            "E:\\internal-review",
            json!({"subagent":{"other":"guardian"}}),
            now,
            "internal approval review",
        );
        let mut collector = Collector::new(home.path().into());
        let usage = collector.collect().remove(&AgentKind::Codex).unwrap();
        assert_eq!(usage.session_id, "main");
        assert_eq!(usage.project, "my-project");
        assert_eq!(usage.message, "message to the user");
        assert_eq!((usage.input, usage.output), (200, 40));
    }

    #[test]
    fn codex_tail_ignores_incomplete_records() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let now = Utc::now();
        writeln!(file, "{}", json!({"timestamp":now,"payload":{}})).unwrap();
        write!(
            file,
            "{{\"timestamp\":\"{}\",",
            now + chrono::Duration::hours(1)
        )
        .unwrap();
        assert_eq!(last_record_time(file.path()), Some(SystemTime::from(now)));
    }

    #[test]
    fn independently_collects_same_project_sessions_including_old_live_log() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".codex/sessions");
        std::fs::create_dir_all(&root).unwrap();
        let now = Utc::now();
        for n in 0..12 {
            write_codex_log(
                &root.join(format!("recent-{n}.jsonl")),
                &format!("recent-{n}"),
                "C:/same",
                serde_json::json!("cli"),
                now,
                &format!("message {n}"),
            );
        }
        let old = root.join("old.jsonl");
        write_codex_log(
            &old,
            "old-live",
            "C:/same",
            serde_json::json!("cli"),
            now - chrono::Duration::days(30),
            "idle for a month",
        );
        let mut collector = Collector::new(home.path().into());
        let logs = collector.collect_logs(&[(AgentKind::Codex, old)]);
        assert_eq!(logs.sessions.len(), 13);
        assert_eq!(
            logs.sessions
                .iter()
                .find(|(_, u)| u.session_id == "old-live")
                .unwrap()
                .1
                .message,
            "idle for a month"
        );
        assert_eq!(logs.events.iter().map(|(_, _, t)| t.0).sum::<u64>(), 1200);
    }

    #[test]
    fn remote_offsets_replacement_half_line_and_local_midnight_reset() {
        let home = tempfile::tempdir().unwrap();
        let mut collector = Collector::new(home.path().into());
        let now = Utc::now();
        let line = serde_json::json!({"timestamp":now,"type":"assistant","sessionId":"session","cwd":"/project","message":{"id":"message","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":10,"output_tokens":3}}}).to_string() + "\n";
        let mut file = crate::probe::RemoteFile {
            path: "/home/user/custom/session.jsonl".into(),
            kind: AgentKind::Claude,
            file_id: "dev:ino".into(),
            size: line.len() as u64 + 10,
            offset: 0,
            data: line.clone() + "{\"partial\"",
            next_offset: None,
            recent_data: String::new(),
        };
        let first = collector.ingest(&[file.clone()]);
        assert!(first.backfilling);
        assert_eq!(first.events.len(), 1);
        assert_eq!(collector.offsets()[&file.path].1, line.len() as u64);
        file.offset = 0;
        file.file_id = "dev:replacement".into();
        file.data = line.clone();
        let second = collector.ingest(&[file.clone()]);
        assert_eq!(second.events[0].2, (10, 3));
        assert_eq!(second.sessions[0].1.session_id, "session");
        collector.day = Local::now().date_naive() - chrono::Duration::days(1);
        assert!(collector.offsets().is_empty());
        let third = collector.ingest(&[file]);
        assert_eq!(third.events.len(), 1);
    }
    #[test]
    fn recent_state_is_independent_of_usage_backfill_and_cannot_double_count_tokens() {
        let home = tempfile::tempdir().unwrap();
        let mut collector = Collector::new(home.path().into());
        let now = Utc::now();
        let metadata = json!({"timestamp":now - chrono::Duration::hours(2),"type":"session_meta","payload":{"id":"live","cwd":"/project"}});
        let started = json!({"timestamp":now - chrono::Duration::hours(1),"type":"event_msg","payload":{"type":"task_started"}});
        let tokens = json!({"timestamp":now,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":20}}}});
        let completed =
            json!({"timestamp":now,"type":"event_msg","payload":{"type":"task_complete"}});
        let lines = |values: &[serde_json::Value]| {
            values
                .iter()
                .map(|v| v.to_string() + "\n")
                .collect::<String>()
        };
        let mut file = crate::probe::RemoteFile {
            path: "/rollout-live.jsonl".into(),
            kind: AgentKind::Codex,
            file_id: "inode".into(),
            size: 3_000_000,
            offset: 0,
            next_offset: Some(2_000_000),
            data: lines(&[metadata.clone(), started]),
            recent_data: lines(&[metadata, tokens.clone(), completed.clone()]),
        };
        let first = collector.ingest(&[file.clone()]);
        assert!(first.backfilling);
        assert!(first.sessions[0].1.waiting);
        assert!(!first.sessions[0].1.working);
        assert_eq!(first.sessions[0].1.last_activity, Some(now));
        assert!(first.events.is_empty());
        assert_eq!(collector.offsets()[&file.path].1, 2_000_000);

        // Replaying the independent tail never contributes to the usage ledger.
        file.offset = 2_000_000;
        file.data.clear();
        let repeated = collector.ingest(&[file.clone()]);
        assert!(repeated.events.is_empty());
        assert!(repeated.sessions[0].1.waiting);

        // Historical ingestion counts the event once when its cursor reaches it.
        file.data = lines(&[tokens, completed]);
        file.next_offset = Some(file.size);
        let final_read = collector.ingest(&[file.clone()]);
        assert!(!final_read.backfilling);
        assert_eq!(final_read.events.len(), 1);
        assert_eq!(final_read.sessions[0].1.input, 100);
        assert_eq!(final_read.sessions[0].1.output, 20);
        file.offset = file.size;
        file.data.clear();
        let again = collector.ingest(&[file.clone()]);
        assert_eq!(again.events.len(), 1);
        assert_eq!(again.events[0].2, (100, 20));

        // Replacement must reset the previous file's tail and accounting.
        file.file_id = "replacement".into();
        file.offset = 0;
        file.next_offset = Some(0);
        file.recent_data.clear();
        let replaced = collector.ingest(&[file]);
        assert!(replaced.events.is_empty());
        assert!(!replaced.sessions[0].1.waiting);
    }
    #[test]
    fn codex_cumulative_dedup_and_turn_boundaries() {
        let mut s = Session::default();
        let today = Local::now().date_naive();
        let ts = Local::now().to_rfc3339();
        apply(
            &mut s,
            AgentKind::Codex,
            &json!({"timestamp":ts,"type":"session_meta","payload":{"id":"a","cwd":"E:\\Source\\test"}}),
            today,
        );
        assert_eq!(s.usage.project, "test");
        for _ in 0..2 {
            apply(
                &mut s,
                AgentKind::Codex,
                &json!({"timestamp":ts,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":20},"last_token_usage":{"input_tokens":100,"output_tokens":20}}}}),
                today,
            );
        }
        assert_eq!(
            s.tokens.values().copied().collect::<Vec<_>>(),
            vec![(100, 20)]
        );
        apply(
            &mut s,
            AgentKind::Codex,
            &json!({"timestamp":ts,"payload":{"type":"function_call","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"build\",\"status\":\"in_progress\"}]}"}}),
            today,
        );
        assert_eq!(s.usage.plan.as_ref().unwrap().current, 1);
        apply(
            &mut s,
            AgentKind::Codex,
            &json!({"timestamp":ts,"payload":{"type":"task_complete","last_agent_message":"done"}}),
            today,
        );
        assert!(s.usage.waiting);
        assert!(s.usage.plan.is_none());
        apply(
            &mut s,
            AgentKind::Codex,
            &json!({"timestamp":ts,"payload":{"type":"user_message"}}),
            today,
        );
        assert!(s.usage.working);
        assert!(!s.usage.waiting);
    }
    #[test]
    fn partial_jsonl_and_claude_cache_tokens() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut s = Session::default();
        let day = Local::now().date_naive();
        let ts = Local::now().to_rfc3339();
        let record=json!({"timestamp":ts,"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"你好"}],"usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30,"output_tokens":5}}}).to_string();
        file.write_all(record.as_bytes()).unwrap();
        read_new(file.path(), &mut s, AgentKind::Claude, day).unwrap();
        assert_eq!(s.offset, 0);
        file.write_all(b"\n").unwrap();
        read_new(file.path(), &mut s, AgentKind::Claude, day).unwrap();
        assert_eq!(s.tokens["m"], (60, 5));
        assert_eq!(s.usage.message, "你好");
        read_new(file.path(), &mut s, AgentKind::Claude, day).unwrap();
        assert_eq!(s.tokens.len(), 1);
    }
    #[test]
    fn collector_deduplicates_forks_and_handles_truncation() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".claude/projects/project");
        std::fs::create_dir_all(&root).unwrap();
        let ts = Local::now().to_rfc3339();
        let record = format!(
            "{}\n",
            json!({"timestamp":ts,"type":"assistant","cwd":"C:\\project","message":{"id":"same","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":10,"output_tokens":2}}})
        );
        for name in ["a.jsonl", "b.jsonl"] {
            std::fs::write(root.join(name), &record).unwrap();
        }
        let mut collector = Collector::new(home.path().into());
        assert_eq!(collector.collect()[&AgentKind::Claude].input, 10);
        std::fs::write(root.join("a.jsonl"), "").unwrap();
        std::fs::write(root.join("b.jsonl"), "").unwrap();
        assert_eq!(collector.collect()[&AgentKind::Claude].input, 0);
    }
}
