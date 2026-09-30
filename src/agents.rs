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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Clone, Default)]
pub struct Plan {
    pub current: usize,
    pub total: usize,
    pub completed: usize,
    pub text: String,
}
#[derive(Clone, Default)]
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
    last_event: Option<SystemTime>,
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
    pub fn collect(&mut self) -> HashMap<AgentKind, Usage> {
        let day = Local::now().date_naive();
        if self.day != day {
            self.sessions.clear();
            self.day = day;
        }
        let mut result = HashMap::new();
        let mut present = HashSet::new();
        let mut discovered = HashSet::new();
        for kind in AgentKind::ALL {
            let root = self.home.join(match kind {
                AgentKind::Claude => ".claude/projects",
                AgentKind::Codex => ".codex/sessions",
                AgentKind::Cursor => ".cursor/projects",
            });
            let mut latest: Option<(SystemTime, Usage, String)> = None;
            let mut daily: HashMap<String, (u64, u64)> = HashMap::new();
            let mut quota: Option<(String, Option<f64>, Option<i64>)> = None;
            let mut files = Vec::new();
            for entry in walkdir::WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                let path = entry.path();
                if !entry.file_type().is_file() || path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let relative = path.strip_prefix(&root).unwrap_or(path);
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
                let Ok(metadata) = entry.metadata() else {
                    continue;
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
            files.sort_unstable_by_key(|(_, _, _, activity)| std::cmp::Reverse(*activity));
            // Read today's active logs plus recent fallback sessions. Activity
            // comes from the transcript on Codex, not NTFS write timestamps.
            for (index, (path, size, modified, activity)) in files.into_iter().enumerate() {
                if index >= 8 && DateTime::<Local>::from(activity).date_naive() < self.day {
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
                session.modified = Some(modified);
                if size > session.offset {
                    let _ = read_new(path, session, kind, self.day);
                }
                for (id, tokens) in &session.tokens {
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
                if latest.as_ref().is_none_or(|v| active_at > v.0) {
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
                    latest = Some((active_at, usage, sid));
                }
            }
            let (mut usage, sid) = latest.map(|(_, u, s)| (u, s)).unwrap_or_default();
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
            result.insert(kind, usage);
        }
        self.sessions.retain(|p, _| present.contains(p));
        self.stamps.retain(|p, _| discovered.contains(p));
        result
    }
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

fn read_new(
    path: &Path,
    session: &mut Session,
    kind: AgentKind,
    day: NaiveDate,
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(session.offset))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    // Limit one pass so a large initial transcript cannot pin the collector forever.
    let mut consumed = 0;
    while consumed < 16 * 1024 * 1024 {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        session.offset += n as u64;
        consumed += n;
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            apply(session, kind, &value, day);
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
    let timestamp = string(&v["timestamp"]);
    let record_time = DateTime::parse_from_rfc3339(timestamp).ok();
    let today = record_time.is_some_and(|t| t.with_timezone(&Local).date_naive() == day);
    if let Some(at) = record_time.map(SystemTime::from) {
        session.last_event = Some(session.last_event.map_or(at, |previous| previous.max(at)));
    }
    if !string(&v["cwd"]).is_empty() {
        session.usage.project = project(string(&v["cwd"]));
    }
    if kind == AgentKind::Codex {
        let p = &v["payload"];
        let ty = string(&p["type"]);
        if string(&v["type"]) == "session_meta" {
            session.session_id = string(&p["id"]).to_owned();
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
            _ if ty.contains("approval_request") => state(session, true, timestamp),
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
            if let Some(blocks) = msg["content"].as_array() {
                for block in blocks {
                    if string(&block["type"]) == "tool_use" {
                        tool = true;
                        if string(&block["name"]) == "TodoWrite" {
                            session.usage.plan = plan(&block["input"]["todos"]);
                        }
                    }
                }
            }
            state(session, !tool, timestamp);
            if !tool {
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
