//! Budgeted hidden child processes isolate queries that can block in the OS.
use crate::{
    agents::AgentKind,
    session::{InstanceKey, LiveInstance, OpenState, SessionKey},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[cfg(any(windows, test))]
mod claude;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProbeResult {
    pub origin_id: String,
    pub user: String,
    pub complete: bool,
    #[serde(default)]
    pub backfilling: bool,
    pub instances: Vec<LiveInstance>,
    pub files: Vec<RemoteFile>,
    pub logs: Vec<(AgentKind, PathBuf)>,
    pub errors: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteFile {
    pub path: String,
    pub kind: AgentKind,
    pub file_id: String,
    pub size: u64,
    pub offset: u64,
    pub data: String,
    /// Physical byte cursor when the guest sends compact JSONL records.
    #[serde(default)]
    pub next_offset: Option<u64>,
    /// Latest complete records of an identified live session, independent of usage backfill.
    #[serde(default)]
    pub recent_data: String,
}
pub fn hidden(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let _ = command;
}
pub fn run(
    command: &mut Command,
    input: Option<&[u8]>,
    stop: &AtomicBool,
) -> anyhow::Result<Vec<u8>> {
    hidden(command);
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let stderr = child.stderr.take().unwrap();
    let error_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.take(65536).read_to_end(&mut bytes).map(|_| bytes)
    });
    let writer = input.map(|input| {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        let bytes = input.to_vec();
        std::thread::spawn(move || stdin.write_all(&bytes))
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if stop.load(Ordering::Relaxed) || start.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Killing the direct child may leave inherited pipes open in WSL. Never
    // join an unfinished pipe reader; the guest also enforces its own budget.
    anyhow::ensure!(status.is_some(), "probe timed out or cancelled");
    let deadline = Instant::now() + Duration::from_millis(150);
    while (!reader.is_finished() || !error_reader.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::ensure!(reader.is_finished(), "probe output pipe did not close");
    anyhow::ensure!(error_reader.is_finished(), "probe error pipe did not close");
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("probe reader failed"))??;
    let errors = error_reader
        .join()
        .map_err(|_| anyhow::anyhow!("probe error reader failed"))??;
    anyhow::ensure!(
        status.unwrap().success(),
        "{} exited with {}: {}",
        command.get_program().to_string_lossy(),
        status.unwrap(),
        decode_wsl(if errors.is_empty() { &bytes } else { &errors })
            .trim()
            .chars()
            .take(1000)
            .collect::<String>()
    );
    if let Some(writer) = writer {
        anyhow::ensure!(writer.is_finished(), "probe input pipe did not close");
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("probe writer failed"))??;
    }
    anyhow::ensure!(bytes.len() <= 1024 * 1024, "probe output exceeds 1 MiB");
    Ok(bytes)
}
pub fn decode_wsl(bytes: &[u8]) -> String {
    if bytes.contains(&0) || bytes.starts_with(&[0xff, 0xfe]) {
        String::from_utf16_lossy(
            &bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .collect::<Vec<_>>(),
        )
        .trim_start_matches('\u{feff}')
        .to_owned()
    } else {
        String::from_utf8_lossy(bytes)
            .trim_start_matches('\u{feff}')
            .to_owned()
    }
}

#[cfg(windows)]
pub fn registrations() -> anyhow::Result<Vec<(String, String)>> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    let root = match RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Lxss")
    {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut result = Vec::new();
    for id in root.enum_keys() {
        let id = id?;
        let key = root.open_subkey(&id)?;
        if key.get_value::<u32, _>("Version")? == 2 {
            result.push((id, key.get_value("DistributionName")?));
        }
    }
    Ok(result)
}
#[cfg(not(windows))]
pub fn registrations() -> anyhow::Result<Vec<(String, String)>> {
    Ok(vec![])
}
pub fn running(stop: &AtomicBool) -> anyhow::Result<Vec<String>> {
    let bytes = run(
        Command::new("wsl.exe").args(["--list", "--running", "--quiet"]),
        None,
        stop,
    )?;
    Ok(decode_wsl(&bytes)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}
pub fn guest(
    distro: &str,
    user: Option<&str>,
    registration: &str,
    offsets: &HashMap<String, (String, u64)>,
    cursor: u64,
    stop: &AtomicBool,
) -> anyhow::Result<ProbeResult> {
    anyhow::ensure!(
        running(stop)?.iter().any(|d| d == distro),
        "WSL distribution is stopped"
    );
    // Recheck narrows the stop race; ordinary wsl.exe cannot eliminate it.
    let mut command = Command::new("wsl.exe");
    command.args(["--distribution", distro]);
    if let Some(user) = user {
        command.args(["--user", user]);
    }
    command.args([
        "--exec",
        "python3",
        "-c",
        include_str!("../scripts/wsl_probe.py"),
    ]);
    let now = chrono::Local::now();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .earliest()
        .map_or(now.timestamp() - 36 * 3600, |at| at.timestamp());
    let request = serde_json::json!({"registration": registration, "offsets": offsets, "cursor": cursor, "day_start": midnight});
    let bytes = run(&mut command, Some(&serde_json::to_vec(&request)?), stop)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(any(windows, test))]
fn codex_role(args: &[String]) -> &'static str {
    if args.get(1).is_some_and(|a| a == "app-server") {
        if args[2..]
            .iter()
            .any(|a| a == "daemon" || a == "pid-update-loop")
        {
            "updater"
        } else {
            "server"
        }
    } else {
        "cli"
    }
}

#[cfg(windows)]
pub fn windows_probe() -> anyhow::Result<ProbeResult> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    let current = system
        .process(sysinfo::Pid::from_u32(std::process::id()))
        .ok_or_else(|| anyhow::anyhow!("current process not found"))?;
    let uid = current
        .user_id()
        .ok_or_else(|| anyhow::anyhow!("current user SID unavailable"))?;
    let origin_id = format!("windows:{}", windows_sid()?);
    let mut result = ProbeResult {
        origin_id: origin_id.clone(),
        user: std::env::var("USERNAME").unwrap_or_default(),
        complete: true,
        ..Default::default()
    };
    let candidates: HashMap<_, _> = system
        .processes()
        .iter()
        .filter_map(|(pid, p)| {
            if p.user_id() != Some(uid) {
                return None;
            }
            let name = p.name().to_string_lossy().to_lowercase();
            if name == "codex.exe"
                && codex_role(
                    &p.cmd()
                        .iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                ) == "updater"
            {
                return None;
            }
            let args = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            let kind = if name == "codex.exe"
                || (name == "node.exe"
                    && (args.contains("@openai/codex") || args.contains("@openai\\codex")))
            {
                AgentKind::Codex
            } else if name == "claude.exe"
                || (name == "node.exe"
                    && (args.contains("claude-code") || args.contains("claude.js")))
            {
                AgentKind::Claude
            } else {
                return None;
            };
            Some((*pid, kind))
        })
        .collect();
    let home = directories::UserDirs::new()
        .ok_or_else(|| anyhow::anyhow!("home directory unavailable"))?;
    let domain = std::env::var("COMPUTERNAME")
        .ok()
        .map(|name| format!("win32:{}", name.to_lowercase()));
    for (pid, kind) in &candidates {
        // Prefer the actual CLI child to its package launcher.
        if system
            .processes()
            .values()
            .any(|p| p.parent() == Some(*pid) && candidates.get(&p.pid()) == Some(kind))
        {
            continue;
        }
        let process = &system.processes()[pid];
        let shared_server = *kind == AgentKind::Codex
            && codex_role(
                &process
                    .cmd()
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            ) == "server";
        let creation = process_time(pid.as_u32());
        let identity_confirmed = creation.is_ok();
        let started = creation.unwrap_or(process.start_time());
        let key = InstanceKey {
            origin_id: origin_id.clone(),
            boot_id: format!("{}", System::boot_time()),
            pid: pid.as_u32(),
            process_started_at: started,
        };
        let mut paths = vec![];
        let mut error = None;
        if *kind == AgentKind::Codex {
            match windows_handles::rollouts(pid.as_u32(), started) {
                Ok(found) => paths = found,
                Err(e) => {
                    result.complete = false;
                    result.errors.push(e.to_string());
                    error = Some(e.to_string());
                }
            }
        }
        let mut ids: HashMap<String, PathBuf> = HashMap::new();
        let mut auxiliary = false;
        for held in paths {
            result.logs.push((*kind, held.path.clone()));
            if let Some(value) = held.metadata {
                if is_auxiliary(&value) {
                    auxiliary = true;
                } else if let Some(id) = value["payload"]["id"].as_str().filter(|id| !id.is_empty())
                {
                    ids.insert(id.into(), held.path);
                }
            }
        }
        if auxiliary && ids.is_empty() {
            continue;
        }
        let native = if *kind == AgentKind::Claude && identity_confirmed {
            let root = process
                .environ()
                .iter()
                .find_map(|entry| {
                    let entry = entry.to_string_lossy();
                    let (name, value) = entry.split_once('=')?;
                    (name.eq_ignore_ascii_case("CLAUDE_CONFIG_DIR") && !value.is_empty())
                        .then(|| PathBuf::from(value))
                })
                .unwrap_or_else(|| home.home_dir().join(".claude"));
            let root = if root.is_absolute() {
                root
            } else {
                process.cwd().unwrap_or(home.home_dir()).join(root)
            };
            match claude::native_session(&root, key.pid, started, domain.as_deref()).and_then(
                |native| {
                    anyhow::ensure!(
                        process_time(key.pid)? == started,
                        "Claude process identity changed"
                    );
                    Ok(native)
                },
            ) {
                Ok(native) => native,
                Err(e) => {
                    result.complete = false;
                    let message = format!("Claude 原生会话登记：{e}");
                    result.errors.push(message.clone());
                    error = Some(message);
                    None
                }
            }
        } else {
            None
        };
        let sid = if *kind == AgentKind::Claude {
            native.as_ref().map(|n| n.session_id.clone())
        } else if ids.len() == 1 && !shared_server {
            ids.keys().next().cloned()
        } else {
            None
        };
        if let Some(path) = native.as_ref().and_then(|n| n.log_path.as_ref()) {
            result.logs.push((*kind, path.clone()));
        }
        for path in ids.values() {
            result.logs.push((*kind, path.clone()));
        }
        let shared_session_keys = if shared_server {
            let mut keys: Vec<_> = ids
                .keys()
                .map(|sid| SessionKey {
                    origin_id: origin_id.clone(),
                    agent_kind: *kind,
                    native_session_id: sid.clone(),
                })
                .collect();
            keys.sort();
            keys
        } else {
            vec![]
        };
        let unlinked_error = (sid.is_none() && shared_session_keys.is_empty()).then(|| {
            if *kind == AgentKind::Claude {
                "Claude 尚未提供有效的原生会话登记".into()
            } else {
                "会话未关联或存在多个主会话候选".into()
            }
        });
        result.instances.push(LiveInstance {
            instance_key: key,
            agent_kind: *kind,
            session_key: sid.map(|sid| SessionKey {
                origin_id: origin_id.clone(),
                agent_kind: *kind,
                native_session_id: sid,
            }),
            shared_session_keys,
            native_work_state: native.and_then(|n| n.work_state),
            last_verified_at: chrono::Utc::now().timestamp(),
            open_state: if identity_confirmed {
                OpenState::Open
            } else {
                OpenState::Unconfirmed
            },
            error: error.or(unlinked_error),
        });
    }
    Ok(result)
}
#[cfg(test)]
fn rollout_metadata(path: &std::path::Path) -> Option<serde_json::Value> {
    rollout_metadata_file(std::fs::File::open(path).ok()?)
}
fn rollout_metadata_file(file: std::fs::File) -> Option<serde_json::Value> {
    use std::io::BufRead;
    let mut bytes = Vec::new();
    std::io::BufReader::new(file.take(65537))
        .read_until(b'\n', &mut bytes)
        .ok()?;
    if bytes.len() > 65536 || bytes.last() != Some(&b'\n') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    (value["type"] == "session_meta").then_some(value)
}
fn is_auxiliary(value: &serde_json::Value) -> bool {
    value["payload"]["source"].get("subagent").is_some() || value["payload"]["source"] == "subagent"
}
#[cfg(test)]
pub fn metadata_id(path: &std::path::Path) -> Option<String> {
    let value = rollout_metadata(path)?;
    if is_auxiliary(&value) {
        return None;
    }
    value["payload"]["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
#[cfg(windows)]
mod windows_handles;
#[cfg(windows)]
pub use windows_handles::process_time;

#[cfg(windows)]
fn windows_sid() -> anyhow::Result<String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut token = std::ptr::null_mut();
    anyhow::ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0,
        "current user token unavailable"
    );
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    let mut bytes = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    let success = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            bytes.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    anyhow::ensure!(success != 0, "current user SID unavailable");
    let user = unsafe { &*bytes.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = std::ptr::null_mut();
    anyhow::ensure!(
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } != 0,
        "SID conversion failed"
    );
    let mut len = 0;
    unsafe {
        while *sid.add(len) != 0 {
            len += 1;
        }
    }
    let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, len) });
    unsafe {
        LocalFree(sid.cast());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_server_and_updater_are_distinct_from_cli() {
        let role =
            |args: &[&str]| codex_role(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            role(&["codex.exe", "app-server", "daemon", "pid-update-loop"]),
            "updater"
        );
        assert_eq!(
            role(&[
                "codex",
                "app-server",
                "--listen",
                "unix://",
                "--managed-daemon"
            ]),
            "server"
        );
        assert_eq!(role(&["codex", "resume", "session"]), "cli");
    }
    #[test]
    fn wsl_output_supports_utf16_bom_and_utf8_unicode_names() {
        let names = "Ubuntu\r\n\r\n开发环境\n";
        let wide: Vec<u8> = std::iter::once(0xfeff)
            .chain(names.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_wsl(&wide), names);
        assert_eq!(decode_wsl(names.as_bytes()), names);
    }
    #[test]
    fn bounded_metadata_reader_rejects_auxiliary_and_partial_records() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"main\"}}\n",
        )
        .unwrap();
        assert_eq!(metadata_id(file.path()).as_deref(), Some("main"));
        std::fs::write(file.path(), b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"aux\",\"source\":{\"subagent\":{}}}}\n").unwrap();
        assert!(metadata_id(file.path()).is_none());
        std::fs::write(
            file.path(),
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"main\"}}",
        )
        .unwrap();
        assert!(metadata_id(file.path()).is_none());
    }
    #[cfg(windows)]
    #[test]
    fn child_cancellation_is_bounded_and_current_creation_identity_is_exact() {
        let cancelled = AtomicBool::new(true);
        let started = Instant::now();
        let result = run(
            Command::new("powershell.exe").args([
                "-NoProfile",
                "-Command",
                "Start-Sleep -Seconds 10",
            ]),
            None,
            &cancelled,
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(process_time(std::process::id()).unwrap() > 100_000_000_000_000_000);
        assert!(windows_sid().unwrap().starts_with("S-1-"));
        let error = run(
            Command::new("cmd.exe").args(["/c", "echo explicit-probe-error 1>&2 & exit /b 7"]),
            None,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("explicit-probe-error"));
        assert!(error.to_string().contains("7"));
    }
}
