//! Read-only Claude native PID registrations; no terminal integration required.
use crate::session::WorkState;
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    pid: u32,
    session_id: String,
    proc_start: String,
    #[serde(default)]
    pid_domain: Option<String>,
    #[serde(default)]
    status: String,
}

pub struct NativeSession {
    pub session_id: String,
    pub log_path: Option<PathBuf>,
    pub work_state: Option<WorkState>,
}

fn registration(path: &Path) -> Result<Option<Registration>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "Claude registration exceeds 64 KiB");
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn verify(record: &Registration, pid: u32, started: u64, domain: Option<&str>) -> Result<()> {
    ensure!(
        record.pid == pid && record.proc_start == started.to_string(),
        "Claude registration process identity mismatch"
    );
    if let (Some(record_domain), Some(domain)) = (&record.pid_domain, domain) {
        ensure!(
            record_domain.eq_ignore_ascii_case(domain),
            "Claude registration PID domain mismatch"
        );
    }
    ensure!(
        !record.session_id.is_empty()
            && record.session_id.len() <= 128
            && record
                .session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid Claude session identity"
    );
    Ok(())
}

fn transcript_matches(path: &Path, session_id: &str) -> Result<bool> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    // Bound both header and tail reads; a large initial user message must not
    // hide the identity in subsequent complete records forever.
    for offset in [0, size.saturating_sub(1024 * 1024)] {
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new((&mut file).take(1024 * 1024));
        let mut line = Vec::new();
        if offset != 0 {
            reader.read_until(b'\n', &mut line)?;
        }
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.last() != Some(&b'\n') {
                continue;
            }
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line)
                && value["sessionId"].as_str() == Some(session_id)
            {
                return Ok(true);
            }
        }
        if size <= 1024 * 1024 {
            break;
        }
    }
    Ok(false)
}

pub fn native_session(
    root: &Path,
    pid: u32,
    started: u64,
    domain: Option<&str>,
) -> Result<Option<NativeSession>> {
    let path = root.join("sessions").join(format!("{pid}.json"));
    let Some(record) = registration(&path)? else {
        return Ok(None);
    };
    verify(&record, pid, started, domain)?;
    let mut log_path = None;
    let projects = match std::fs::read_dir(root.join("projects")) {
        Ok(projects) => Some(projects),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    for project in projects.into_iter().flatten() {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        let candidate = project.path().join(format!("{}.jsonl", record.session_id));
        if !candidate.try_exists()? {
            continue;
        }
        ensure!(log_path.is_none(), "multiple Claude transcript candidates");
        ensure!(
            transcript_matches(&candidate, &record.session_id)?,
            "Claude transcript identity could not be verified"
        );
        log_path = Some(candidate);
    }
    // CLI /resume can replace the record while its old transcript is inspected.
    let current = registration(&path)?
        .ok_or_else(|| anyhow::anyhow!("Claude registration disappeared during discovery"))?;
    verify(&current, pid, started, domain)?;
    ensure!(
        current.session_id == record.session_id,
        "Claude session changed during discovery"
    );
    let work_state = match current.status.as_str() {
        "busy" => Some(WorkState::Working),
        "idle" => Some(WorkState::Idle),
        _ => None,
    };
    Ok(Some(NativeSession {
        session_id: current.session_id,
        log_path,
        work_state,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(root: &Path, status: &str) -> PathBuf {
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::fs::create_dir_all(root.join("projects/project")).unwrap();
        let path = root.join("sessions/42.json");
        std::fs::write(
            &path,
            json!({"pid":42,"sessionId":"session-a","procStart":"123", "status":status,
                "pidDomain":"win32:test","updatedAt":1})
            .to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn native_identity_and_status_do_not_require_recent_messages_or_heartbeat() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "busy");
        let log = root.path().join("projects/project/session-a.jsonl");
        std::fs::write(&log, "{\"sessionId\":\"session-a\"}\n").unwrap();
        let session = native_session(root.path(), 42, 123, Some("win32:test"))
            .unwrap()
            .unwrap();
        assert_eq!(session.session_id, "session-a");
        assert_eq!(session.log_path, Some(log));
        assert_eq!(session.work_state, Some(WorkState::Working));
        fixture(root.path(), "idle");
        assert_eq!(
            native_session(root.path(), 42, 123, None)
                .unwrap()
                .unwrap()
                .work_state,
            Some(WorkState::Idle)
        );
    }

    #[test]
    fn mismatched_process_domain_and_session_paths_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path(), "idle");
        assert!(native_session(root.path(), 42, 124, Some("win32:test")).is_err());
        assert!(native_session(root.path(), 42, 123, Some("win32:other")).is_err());
        std::fs::write(
            &path,
            json!({"pid":99,"sessionId":"session-a","procStart":"123"}).to_string(),
        )
        .unwrap();
        assert!(native_session(root.path(), 42, 123, None).is_err());
        std::fs::write(
            &path,
            json!({"pid":42,"sessionId":"../outside","procStart":"123"}).to_string(),
        )
        .unwrap();
        assert!(native_session(root.path(), 42, 123, None).is_err());
    }

    #[test]
    fn missing_transcripts_do_not_borrow_another_session_and_unknown_status_is_not_idle() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            native_session(root.path(), 42, 123, None)
                .unwrap()
                .is_none()
        );
        fixture(root.path(), "future-status");
        std::fs::write(
            root.path().join("projects/project/other.jsonl"),
            "{\"sessionId\":\"other\"}\n",
        )
        .unwrap();
        let session = native_session(root.path(), 42, 123, None).unwrap().unwrap();
        assert!(session.log_path.is_none());
        assert!(session.work_state.is_none());
        let log = root.path().join("projects/project/session-a.jsonl");
        std::fs::write(&log, "{\"sessionId\":\"other\"}\n").unwrap();
        assert!(native_session(root.path(), 42, 123, None).is_err());
    }

    #[test]
    fn transcript_tail_can_verify_identity_after_large_initial_records() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "idle");
        let log = root.path().join("projects/project/session-a.jsonl");
        let mut bytes = vec![b'x'; 2 * 1024 * 1024];
        bytes.extend_from_slice(b"\n{\"sessionId\":\"session-a\"}\n");
        std::fs::write(log, bytes).unwrap();
        assert!(
            native_session(root.path(), 42, 123, None)
                .unwrap()
                .unwrap()
                .log_path
                .is_some()
        );
    }

    #[test]
    fn malformed_or_oversized_registrations_are_not_trusted() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path(), "busy");
        std::fs::write(&path, b"{\"pid\":42").unwrap();
        assert!(native_session(root.path(), 42, 123, None).is_err());
        std::fs::write(&path, vec![b' '; 65537]).unwrap();
        assert!(native_session(root.path(), 42, 123, None).is_err());
    }
}
