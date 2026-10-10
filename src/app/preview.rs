//! Private inherited pipes connect a disposable GUI to the persistent LCD runtime.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        OnceLock,
        mpsc::{self, Receiver},
    },
};

const MAX_METADATA: usize = 8 * 1024 * 1024;
const MAX_COMMAND: usize = 1024 * 1024;
const FRAME_BYTES: usize = 1920 * 480 * 4;
const LOG_BYTES: u64 = 256 * 1024;
const METADATA_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) enum UiRequest {
    Preview,
    Settings,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Request {
    pub(super) id: u64,
    pub(super) action: UiRequest,
}
#[derive(Serialize, Deserialize)]
pub(super) enum ClientCommand {
    Settings(Settings),
    Advance(isize),
    View(ViewState),
    Quit,
    HandledRequest(u64),
}
#[derive(Serialize, Deserialize)]
struct Packet {
    settings: Option<Settings>,
    settings_path: Option<PathBuf>,
    agents: Option<AgentSnapshot>,
    output: Output,
    error: Option<String>,
    requests: Vec<Request>,
    has_frame: bool,
    can_hide: bool,
    shutdown: bool,
}

pub(super) fn diagnostic(settings_path: &std::path::Path, message: &str) {
    // Two bounded files retain errors even when Explorer supplies no stderr handle.
    static LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = LOG_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let path = settings_path.with_file_name("preview.log");
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let entry = format!(
            "{} {message}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        // Individual stderr reads are bounded; also bound caller-provided error chains.
        let bytes = entry.as_bytes();
        let bytes = &bytes[..bytes.len().min(16 * 1024)];
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) + bytes.len() as u64 > LOG_BYTES {
            let previous = path.with_extension("log.1");
            if previous.exists() {
                std::fs::remove_file(&previous)?;
            }
            std::fs::rename(&path, previous)?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?
            .write_all(bytes)
    })();
    if let Err(error) = result {
        eprintln!("Preview logging failed: {error}; {message}");
    }
}
pub(super) fn report(ui: &UiState, message: &str) {
    diagnostic(&ui.settings_path, message);
    let mut bounded = message.to_owned();
    while bounded.len() > 4096 {
        bounded.pop();
    }
    *ui.error.lock().unwrap() = format!(
        "{bounded}\n日志：{}",
        ui.settings_path.with_file_name("preview.log").display()
    );
}

fn send_error(ui: &UiState, error: &anyhow::Error) {
    let message = format!("Preview send failed: {error:#}");
    if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
    {
        // Closing the GUI normally also breaks its input pipe. The supervisor
        // reports a nonzero child exit separately; preserve unrelated UI errors.
        diagnostic(&ui.settings_path, &message);
    } else {
        report(ui, &message);
    }
}

#[derive(Default)]
pub(super) struct SpawnRetry {
    failures: u32,
    next: Option<Instant>,
}
impl SpawnRetry {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
    pub(super) fn ready(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }
    pub(super) fn failed(&mut self, now: Instant) -> bool {
        self.failures += 1;
        self.next = Some(now + Duration::from_secs(1 << self.failures.min(3)));
        self.failures < 3
    }
}
fn write_json(writer: &mut impl Write, value: &impl Serialize, limit: usize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(bytes.len() <= limit, "Preview message exceeds size limit");
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(reader: &mut impl Read, limit: usize) -> Result<T> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header) as usize;
    anyhow::ensure!(
        size > 0 && size <= limit,
        "Invalid preview message size: {size}"
    );
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
pub(super) fn send_command(writer: &mut impl Write, command: &ClientCommand) -> Result<()> {
    write_json(writer, command, MAX_COMMAND)?;
    writer.flush()?;
    Ok(())
}
fn write_packet(writer: &mut impl Write, packet: &Packet) -> Result<()> {
    if packet.has_frame {
        let frame = packet
            .output
            .frame
            .as_ref()
            .context("Missing preview frame")?;
        anyhow::ensure!(
            frame.dimensions() == (1920, 480),
            "Invalid preview dimensions"
        );
    }
    write_json(writer, packet, MAX_METADATA)?;
    if packet.has_frame {
        writer.write_all(packet.output.frame.as_ref().unwrap().as_raw())?;
    }
    writer.flush()?;
    Ok(())
}
fn read_packet(reader: &mut impl Read) -> Result<Packet> {
    let mut packet: Packet = read_json(reader, MAX_METADATA)?;
    if packet.has_frame {
        let mut bytes = vec![0; FRAME_BYTES];
        reader.read_exact(&mut bytes)?;
        packet.output.frame = Some(Arc::new(
            image::RgbaImage::from_raw(1920, 480, bytes).unwrap(),
        ));
    }
    Ok(packet)
}

#[derive(Default)]
struct Changes {
    settings: Vec<u8>,
    agents: Vec<u8>,
    error: Option<String>,
}
impl Changes {
    fn settings(&mut self, settings: Settings, force: bool) -> Result<Option<Settings>> {
        let value = serde_json::to_vec(&settings)?;
        if !force && self.settings == value {
            return Ok(None);
        }
        self.settings = value;
        Ok(Some(settings))
    }
    fn agents(&mut self, agents: AgentSnapshot) -> Result<Option<AgentSnapshot>> {
        let value = serde_json::to_vec(&agents)?;
        if self.agents == value {
            return Ok(None);
        }
        self.agents = value;
        Ok(Some(agents))
    }
    fn error(&mut self, error: String) -> Option<String> {
        if self.error.as_ref() == Some(&error) {
            return None;
        }
        self.error = Some(error.clone());
        Some(error)
    }
}

#[derive(Default)]
struct PendingRequests {
    next: u64,
    pending: BTreeMap<u64, UiRequest>,
}
impl PendingRequests {
    fn insert(&mut self, request: UiRequest) {
        // Repeated clicks of a kind supersede older requests, bounding queue size.
        self.pending
            .retain(|_, value| std::mem::discriminant(value) != std::mem::discriminant(&request));
        self.next += 1;
        self.pending.insert(self.next, request);
    }
    fn acknowledge(&mut self, id: u64) {
        self.pending.remove(&id);
    }
    fn snapshot(&self) -> Vec<Request> {
        self.pending
            .iter()
            .map(|(&id, &action)| Request { id, action })
            .collect()
    }
}

pub(super) struct PreviewProcess {
    child: Child,
    stop: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    commands: Receiver<ClientCommand>,
    requests: Arc<Mutex<PendingRequests>>,
    ui: UiState,
}
impl PreviewProcess {
    pub(super) fn spawn(runtime: &Runtime, show_settings: bool) -> Result<Self> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--preview-client")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::probe::hidden(&mut command);
        let mut child = command.spawn().context("Starting preview process")?;
        let mut writer = child.stdin.take().unwrap();
        let mut reader = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let closing = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(PendingRequests::default()));
        let ui = runtime.ui.clone();
        let send_ui = ui.clone();
        let writer_stop = stop.clone();
        let writer_closing = closing.clone();
        let writer_requests = requests.clone();
        let send = thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut last_sequence = None;
                let mut changes = Changes::default();
                let mut next_metadata = Instant::now();
                let mut initial = true;
                let mut settings_epoch = 0;
                while !writer_stop.load(Ordering::Relaxed) {
                    // Each lock has its own statement, so no guards overlap.
                    let output = send_ui.output.lock().unwrap().clone();
                    let has_frame = output.preview_frame(last_sequence).is_some();
                    let sequence = output.sequence;
                    let mut settings = None;
                    let mut agents = None;
                    let mut error = None;
                    if Instant::now() >= next_metadata {
                        let latest_settings = send_ui.shared_settings.lock().unwrap().clone();
                        let epoch = send_ui.settings_epoch.load(Ordering::Relaxed);
                        settings = changes.settings(latest_settings, epoch != settings_epoch)?;
                        settings_epoch = epoch;
                        let latest_agents = send_ui.agents.lock().unwrap().clone();
                        agents = changes.agents(latest_agents)?;
                        let latest_error = send_ui.error.lock().unwrap().clone();
                        error = changes.error(latest_error);
                        next_metadata = Instant::now() + METADATA_INTERVAL;
                    }
                    let requests = writer_requests.lock().unwrap().snapshot();
                    let packet = Packet {
                        settings,
                        settings_path: initial.then(|| send_ui.settings_path.clone()),
                        agents,
                        output,
                        error,
                        requests,
                        has_frame,
                        can_hide: send_ui.can_hide,
                        shutdown: writer_closing.load(Ordering::Relaxed),
                    };
                    write_packet(&mut writer, &packet)?;
                    initial = false;
                    if has_frame {
                        last_sequence = Some(sequence);
                    }
                    if packet.shutdown {
                        break;
                    }
                    interruptible_sleep(&writer_stop, Duration::from_millis(67));
                }
                Ok(())
            })();
            if let Err(error) = result
                && !writer_stop.load(Ordering::Relaxed)
            {
                send_error(&send_ui, &error);
            }
            // Dropping ChildStdin sends EOF and closes a client after any IPC failure.
        });
        let (sender, commands) = mpsc::sync_channel(32);
        let read_ui = ui.clone();
        let reader_stop = stop.clone();
        let receive = thread::spawn(move || {
            loop {
                match read_json(&mut reader, MAX_COMMAND) {
                    Ok(command) => {
                        if sender.try_send(command).is_err() {
                            report(
                                &read_ui,
                                "Preview command queue overflow or receiver closed",
                            );
                            break;
                        }
                    }
                    Err(error) => {
                        if !reader_stop.load(Ordering::Relaxed) {
                            diagnostic(
                                &read_ui.settings_path,
                                &format!("Preview command pipe closed: {error:#}"),
                            );
                        }
                        break;
                    }
                }
            }
        });
        let log_path = ui.settings_path.clone();
        let errors = thread::spawn(move || {
            let mut bytes = [0; 4096];
            loop {
                match stderr.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(size) => diagnostic(
                        &log_path,
                        &format!(
                            "Preview stderr: {}",
                            String::from_utf8_lossy(&bytes[..size])
                        ),
                    ),
                    Err(error) => {
                        diagnostic(
                            &log_path,
                            &format!("Reading preview stderr failed: {error}"),
                        );
                        break;
                    }
                }
            }
        });
        let mut process = Self {
            child,
            stop,
            closing,
            workers: vec![send, receive, errors],
            commands,
            requests,
            ui,
        };
        if show_settings {
            process.request(UiRequest::Settings);
        }
        Ok(process)
    }
    pub(super) fn request(&mut self, request: UiRequest) {
        self.requests.lock().unwrap().insert(request);
    }
    pub(super) fn pending_requests(&self) -> Vec<UiRequest> {
        self.requests
            .lock()
            .unwrap()
            .snapshot()
            .into_iter()
            .map(|r| r.action)
            .collect()
    }
    pub(super) fn apply_commands(&self, ui: &UiState) {
        for command in self.commands.try_iter() {
            if let ClientCommand::HandledRequest(id) = command {
                self.requests.lock().unwrap().acknowledge(id);
            } else {
                apply_command(ui, command);
            }
            if ui.shutdown.load(Ordering::Relaxed) {
                break;
            }
        }
    }
    pub(super) fn exited(&mut self) -> Result<bool> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(false);
        };
        self.stop.store(true, Ordering::Relaxed);
        self.reap_workers();
        if !status.success() {
            report(&self.ui, &format!("Preview exited with {status}"));
        }
        Ok(true)
    }
    fn reap_workers(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline && !self.workers.iter().all(JoinHandle::is_finished) {
            thread::sleep(Duration::from_millis(10));
        }
        for worker in self.workers.drain(..) {
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}
fn apply_command(ui: &UiState, command: ClientCommand) {
    match command {
        ClientCommand::Settings(settings) => {
            // Commit to disk before publishing to workers: failed writes preserve the old configuration.
            match settings.save(&ui.settings_path) {
                Ok(()) => *ui.shared_settings.lock().unwrap() = settings,
                Err(error) => report(ui, &format!("Saving settings failed: {error:#}")),
            }
            ui.settings_epoch.fetch_add(1, Ordering::Relaxed);
        }
        ClientCommand::Advance(delta) => ui.view.lock().unwrap().advance(delta, Instant::now()),
        ClientCommand::View(mut view) => {
            view.reset_timer(Instant::now());
            *ui.view.lock().unwrap() = view;
        }
        ClientCommand::Quit => ui.shutdown.store(true, Ordering::Relaxed),
        ClientCommand::HandledRequest(_) => {}
    }
}
impl Drop for PreviewProcess {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    diagnostic(
                        &self.ui.settings_path,
                        &format!("Waiting for preview failed: {error}"),
                    );
                    break;
                }
            }
        }
        self.stop.store(true, Ordering::Relaxed);
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            diagnostic(
                &self.ui.settings_path,
                "Preview did not close gracefully; terminating after timeout",
            );
            if let Err(error) = self.child.kill() {
                diagnostic(
                    &self.ui.settings_path,
                    &format!("Terminating preview failed: {error}"),
                );
            }
        }
        // Successful termination closes the child's pipe endpoints and releases blocked IO.
        // Never wait/join indefinitely if the OS refuses to terminate/query the child.
        self.reap_workers();
        let _ = self.child.try_wait();
    }
}

struct RemoteState {
    ui: UiState,
    seen_request: u64,
}
fn apply_packet(state: &mut RemoteState, mut packet: Packet) {
    if let Some(settings) = packet.settings {
        *state.ui.shared_settings.lock().unwrap() = settings;
    }
    if let Some(agents) = packet.agents {
        *state.ui.agents.lock().unwrap() = agents;
    }
    if let Some(error) = packet.error {
        *state.ui.error.lock().unwrap() = error;
    }
    *state.ui.view.lock().unwrap() = packet.output.view.clone();
    {
        let mut output = state.ui.output.lock().unwrap();
        if !packet.has_frame {
            packet.output.frame = output.frame.take();
        }
        *output = packet.output;
    }
    for request in packet.requests {
        if request.id > state.seen_request {
            state.seen_request = request.id;
            state.ui.requests.lock().unwrap().push(request);
        }
    }
    if packet.shutdown {
        state.ui.shutdown.store(true, Ordering::Relaxed);
    }
    let target = state.ui.repaint_target.lock().unwrap().clone();
    if let Some(target) = target {
        target.ctx.request_repaint();
    }
}
fn remote_ui(first: &Packet) -> Result<UiState> {
    Ok(UiState {
        settings_path: first
            .settings_path
            .clone()
            .context("Missing initial settings path")?,
        shared_settings: Arc::new(Mutex::new(
            first.settings.clone().context("Missing initial settings")?,
        )),
        agents: Arc::default(),
        view: Arc::default(),
        output: Arc::default(),
        shutdown: Arc::new(AtomicBool::new(false)),
        repaint_target: Arc::default(),
        error: Arc::default(),
        settings_epoch: Arc::default(),
        can_hide: first.can_hide,
        requests: Arc::default(),
    })
}
pub fn run_preview_client() -> Result<()> {
    let mut reader = std::io::stdin();
    let first = read_packet(&mut reader).context("Reading initial preview state")?;
    let ui = remote_ui(&first)?;
    let mut receiving = RemoteState {
        ui: ui.clone(),
        seen_request: 0,
    };
    apply_packet(&mut receiving, first);
    thread::spawn(move || {
        loop {
            match read_packet(&mut reader) {
                Ok(packet) => {
                    apply_packet(&mut receiving, packet);
                    if receiving.ui.shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("Preview input pipe closed: {error:#}");
                    break;
                }
            }
        }
        receiving.ui.shutdown.store(true, Ordering::Relaxed);
        let target = receiving.ui.repaint_target.lock().unwrap().clone();
        if let Some(target) = target {
            target.ctx.request_repaint();
        }
    });
    run_preview(&ui, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn packet(frame: bool) -> Packet {
        Packet {
            settings: Some(Settings::default()),
            settings_path: Some("settings.json".into()),
            agents: Some(AgentSnapshot::default()),
            output: Output {
                frame: frame.then(|| {
                    Arc::new(image::RgbaImage::from_pixel(
                        1920,
                        480,
                        image::Rgba([1, 2, 3, 255]),
                    ))
                }),
                sequence: 42,
                ..Default::default()
            },
            error: Some(String::new()),
            requests: vec![Request {
                id: 1,
                action: UiRequest::Settings,
            }],
            has_frame: frame,
            can_hide: true,
            shutdown: false,
        }
    }
    #[test]
    fn frame_and_metadata_roundtrip() {
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &packet(true)).unwrap();
        let decoded = read_packet(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded.output.sequence, 42);
        assert_eq!(
            decoded.output.frame.unwrap().get_pixel(1919, 479).0,
            [1, 2, 3, 255]
        );
        assert!(matches!(decoded.requests[0].action, UiRequest::Settings));
    }
    #[test]
    fn deltas_retain_frame_settings_agents_and_deduplicate_requests() {
        let first = packet(true);
        let ui = remote_ui(&first).unwrap();
        let mut state = RemoteState {
            ui,
            seen_request: 0,
        };
        apply_packet(&mut state, first);
        let frame = state.ui.output.lock().unwrap().frame.clone().unwrap();
        let mut delta = packet(false);
        delta.settings = None;
        delta.agents = None;
        delta.error = None;
        delta.output.status = "LCD reconnecting".into();
        apply_packet(&mut state, delta);
        let output = state.ui.output.lock().unwrap();
        assert_eq!(output.status, "LCD reconnecting");
        assert!(Arc::ptr_eq(output.frame.as_ref().unwrap(), &frame));
        assert_eq!(state.ui.requests.lock().unwrap().len(), 1);
        assert_eq!(
            state.ui.shared_settings.lock().unwrap().brightness,
            Settings::default().brightness
        );
    }
    #[test]
    fn rejects_oversized_and_truncated_messages() {
        assert!(read_packet(&mut Cursor::new(u32::MAX.to_le_bytes())).is_err());
        assert!(read_packet(&mut Cursor::new([10, 0])).is_err());
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &packet(true)).unwrap();
        bytes.pop();
        assert!(read_packet(&mut Cursor::new(bytes)).is_err());
    }
    #[test]
    fn commands_roundtrip_and_parent_persists_settings() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = packet(false);
        first.settings_path = Some(temp.path().join("settings.json"));
        let ui = remote_ui(&first).unwrap();
        let settings = Settings {
            brightness: 7,
            rotate: false,
            ..Default::default()
        };
        let mut bytes = Vec::new();
        send_command(&mut bytes, &ClientCommand::Settings(settings)).unwrap();
        apply_command(
            &ui,
            read_json(&mut Cursor::new(bytes), MAX_COMMAND).unwrap(),
        );
        assert_eq!(ui.shared_settings.lock().unwrap().brightness, 7);
        let saved: Settings =
            serde_json::from_slice(&std::fs::read(&ui.settings_path).unwrap()).unwrap();
        assert_eq!(saved.brightness, 7);
        apply_command(&ui, ClientCommand::Quit);
        assert!(ui.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn normal_pipe_close_preserves_ui_errors_but_protocol_failure_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = packet(false);
        first.settings_path = Some(temp.path().join("settings.json"));
        let ui = remote_ui(&first).unwrap();
        *ui.error.lock().unwrap() = "Existing configuration error".into();
        send_error(
            &ui,
            &std::io::Error::from(std::io::ErrorKind::BrokenPipe).into(),
        );
        assert_eq!(*ui.error.lock().unwrap(), "Existing configuration error");
        send_error(&ui, &anyhow::anyhow!("Preview message exceeds size limit"));
        assert!(ui.error.lock().unwrap().contains("exceeds size limit"));
    }
    #[test]
    fn failed_settings_save_keeps_backend_configuration_and_reports_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = packet(false);
        first.settings_path = Some(temp.path().to_owned());
        let ui = remote_ui(&first).unwrap();
        let previous = ui.shared_settings.lock().unwrap().brightness;
        apply_command(
            &ui,
            ClientCommand::Settings(Settings {
                brightness: 1,
                ..Default::default()
            }),
        );
        assert_eq!(ui.shared_settings.lock().unwrap().brightness, previous);
        assert!(ui.error.lock().unwrap().contains("Saving settings failed"));
    }
    #[test]
    fn metadata_sends_only_changed_content() {
        let mut changes = Changes::default();
        let mut agents = AgentSnapshot::default();
        assert!(changes.agents(agents.clone()).unwrap().is_some());
        assert!(changes.agents(agents.clone()).unwrap().is_none());
        agents.daily_usage.input = 42;
        assert!(changes.agents(agents.clone()).unwrap().is_some());
        assert!(changes.agents(agents).unwrap().is_none());
        assert!(
            changes
                .settings(Settings::default(), false)
                .unwrap()
                .is_some()
        );
        assert!(
            changes
                .settings(Settings::default(), false)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn spawn_failure_retries_are_bounded_and_explicit_request_resets_them() {
        let now = Instant::now();
        let mut retry = SpawnRetry::default();
        assert!(retry.ready(now));
        assert!(retry.failed(now));
        assert!(!retry.ready(now));
        assert!(retry.ready(now + Duration::from_secs(2)));
        assert!(retry.failed(now));
        assert!(!retry.failed(now));
        retry.reset();
        assert!(retry.ready(now));
    }
    #[test]
    fn shutdown_packet_closes_remote_without_quitting_backend() {
        let first = packet(false);
        let ui = remote_ui(&first).unwrap();
        let mut state = RemoteState {
            ui,
            seen_request: 0,
        };
        let mut closing = first;
        closing.shutdown = true;
        apply_packet(&mut state, closing);
        assert!(state.ui.shutdown.load(Ordering::Relaxed));
    }
    #[test]
    fn unacknowledged_requests_survive_exit_and_late_acks_do_not_drop_new_clicks() {
        let mut pending = PendingRequests::default();
        pending.insert(UiRequest::Settings);
        let first = pending.snapshot()[0].id;
        pending.insert(UiRequest::Settings);
        pending.acknowledge(first);
        assert_eq!(pending.snapshot().len(), 1);
        assert!(matches!(pending.snapshot()[0].action, UiRequest::Settings));
        pending.insert(UiRequest::Preview);
        let settings_id = pending.snapshot()[0].id;
        pending.acknowledge(settings_id);
        assert_eq!(pending.snapshot().len(), 1);
        assert!(matches!(pending.snapshot()[0].action, UiRequest::Preview));
    }
    #[test]
    fn failed_save_forces_an_unchanged_configuration_response() {
        let mut changes = Changes::default();
        assert!(
            changes
                .settings(Settings::default(), false)
                .unwrap()
                .is_some()
        );
        assert!(
            changes
                .settings(Settings::default(), false)
                .unwrap()
                .is_none()
        );
        assert!(
            changes
                .settings(Settings::default(), true)
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn shutdown_timeout_releases_a_backpressured_sender() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = packet(false);
        first.settings_path = Some(temp.path().join("settings.json"));
        let ui = remote_ui(&first).unwrap();
        let powershell = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| "C:/Windows".into())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut command = Command::new(powershell);
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::probe::hidden(&mut command);
        let mut child = command.spawn().unwrap();
        let mut writer = child.stdin.take().unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        let worker = thread::spawn(move || {
            let _ = writer.write_all(&vec![0; FRAME_BYTES]);
            done.store(true, Ordering::Relaxed);
        });
        let (_, commands) = mpsc::channel();
        let process = PreviewProcess {
            child,
            stop: Arc::new(AtomicBool::new(false)),
            closing: Arc::new(AtomicBool::new(false)),
            workers: vec![worker],
            commands,
            requests: Arc::default(),
            ui,
        };
        let start = Instant::now();
        drop(process);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "Shutdown must be bounded even with a full pipe"
        );
        assert!(
            finished.load(Ordering::Relaxed),
            "Terminating the client must release the blocked sender"
        );
        assert!(
            std::fs::read_to_string(temp.path().join("preview.log"))
                .unwrap()
                .contains("terminating after timeout")
        );
    }
    #[test]
    fn stderr_log_rotation_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings.json");
        for _ in 0..40 {
            diagnostic(&settings, &"x".repeat(16384));
        }
        for name in ["preview.log", "preview.log.1"] {
            assert!(std::fs::metadata(temp.path().join(name)).unwrap().len() <= LOG_BYTES);
        }
    }
}
