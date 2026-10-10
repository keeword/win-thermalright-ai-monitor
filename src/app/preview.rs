//! Private inherited pipes connect a disposable GUI to the persistent LCD runtime.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
};

const MAX_METADATA: usize = 8 * 1024 * 1024;
const MAX_COMMAND: usize = 1024 * 1024;
const FRAME_BYTES: usize = 1920 * 480 * 4;

#[derive(Serialize, Deserialize)]
pub(super) enum UiRequest {
    Preview,
    Settings,
}

#[derive(Serialize, Deserialize)]
pub(super) enum ClientCommand {
    Settings(Settings),
    Advance(isize),
    View(ViewState),
    Quit,
}

#[derive(Serialize, Deserialize)]
struct Packet {
    settings: Settings,
    settings_path: PathBuf,
    agents: AgentSnapshot,
    output: Output,
    error: String,
    requests: Vec<UiRequest>,
    has_frame: bool,
    can_hide: bool,
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

pub(super) struct PreviewProcess {
    child: Child,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    commands: Receiver<ClientCommand>,
    requests: Arc<Mutex<Vec<UiRequest>>>,
}

impl PreviewProcess {
    pub(super) fn spawn(runtime: &Runtime, show_settings: bool) -> Result<Self> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--preview-client")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        crate::probe::hidden(&mut command);
        let mut child = command.spawn().context("Starting preview process")?;
        let mut writer = child.stdin.take().unwrap();
        let mut reader = child.stdout.take().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(if show_settings {
            vec![UiRequest::Settings]
        } else {
            Vec::new()
        }));
        let output = runtime.output.clone();
        let agents = runtime.agents.clone();
        let settings = runtime.shared_settings.clone();
        let settings_path = runtime.settings_path.clone();
        let error = runtime.error.clone();
        let can_hide = runtime.can_hide();
        let writer_stop = stop.clone();
        let writer_requests = requests.clone();
        let send = thread::spawn(move || {
            let mut last_sequence = None;
            while !writer_stop.load(Ordering::Relaxed) {
                let output = output.lock().unwrap().clone();
                let has_frame = output.frame.is_some() && last_sequence != Some(output.sequence);
                let sequence = output.sequence;
                let packet = Packet {
                    settings: settings.lock().unwrap().clone(),
                    settings_path: settings_path.clone(),
                    agents: agents.lock().unwrap().clone(),
                    output,
                    error: error.clone(),
                    requests: writer_requests.lock().unwrap().drain(..).collect(),
                    has_frame,
                    can_hide,
                };
                if write_packet(&mut writer, &packet).is_err() {
                    break;
                }
                if has_frame {
                    last_sequence = Some(sequence);
                }
                interruptible_sleep(&writer_stop, Duration::from_millis(67));
            }
        });
        // A bounded channel limits memory if a malfunctioning client floods commands.
        let (sender, commands) = mpsc::sync_channel(32);
        let receive = thread::spawn(move || {
            while let Ok(command) = read_json(&mut reader, MAX_COMMAND) {
                if sender.try_send(command).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stop,
            workers: vec![send, receive],
            commands,
            requests,
        })
    }

    pub(super) fn request(&self, request: UiRequest) {
        self.requests.lock().unwrap().push(request);
    }

    pub(super) fn apply_commands(&self, runtime: &Runtime) {
        for command in self.commands.try_iter() {
            apply_command(runtime, command);
        }
    }

    pub(super) fn exited(&mut self) -> Result<bool> {
        if self.child.try_wait()?.is_none() {
            return Ok(false);
        }
        self.stop.store(true, Ordering::Relaxed);
        // Drain the pipe reader before the parent applies a final Quit command.
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        Ok(true)
    }
}

fn apply_command(runtime: &Runtime, command: ClientCommand) {
    match command {
        ClientCommand::Settings(settings) => *runtime.shared_settings.lock().unwrap() = settings,
        ClientCommand::Advance(delta) => {
            runtime.view.lock().unwrap().advance(delta, Instant::now())
        }
        ClientCommand::View(mut view) => {
            view.reset_timer(Instant::now());
            *runtime.view.lock().unwrap() = view;
        }
        ClientCommand::Quit => runtime.shutdown.store(true, Ordering::Relaxed),
    }
}

impl Drop for PreviewProcess {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Termination also unblocks either pipe if the GUI stopped consuming data.
        let _ = self.child.kill();
        let _ = self.child.wait();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

struct RemoteState {
    agents: Arc<Mutex<AgentSnapshot>>,
    view: Arc<Mutex<ViewState>>,
    output: Arc<Mutex<Output>>,
    actions: Arc<Mutex<Vec<TrayAction>>>,
    shutdown: Arc<AtomicBool>,
    repaint: RepaintTarget,
}

impl RemoteState {
    fn new(runtime: &Runtime) -> Self {
        Self {
            agents: runtime.agents.clone(),
            view: runtime.view.clone(),
            output: runtime.output.clone(),
            actions: runtime.remote_actions.clone(),
            shutdown: runtime.shutdown.clone(),
            repaint: runtime.repaint_target.clone(),
        }
    }
}

fn apply_packet(state: &RemoteState, mut packet: Packet) {
    *state.agents.lock().unwrap() = packet.agents;
    *state.view.lock().unwrap() = packet.output.view.clone();
    {
        let mut output = state.output.lock().unwrap();
        if !packet.has_frame {
            packet.output.frame = output.frame.take();
        }
        *output = packet.output;
    }
    state
        .actions
        .lock()
        .unwrap()
        .extend(packet.requests.into_iter().map(|request| match request {
            UiRequest::Preview => TrayAction::Preview,
            UiRequest::Settings => TrayAction::Settings,
        }));
    if let Some(target) = state.repaint.lock().unwrap().clone() {
        target.ctx.request_repaint();
    }
}

fn remote_runtime(first: &Packet) -> Runtime {
    Runtime {
        settings_path: first.settings_path.clone(),
        shared_settings: Arc::new(Mutex::new(first.settings.clone())),
        agents: Arc::default(),
        view: Arc::default(),
        output: Arc::default(),
        stop: Arc::new(AtomicBool::new(false)),
        shutdown: Arc::new(AtomicBool::new(false)),
        workers: Vec::new(),
        repaint_target: Arc::default(),
        error: first.error.clone(),
        remote: true,
        remote_can_hide: first.can_hide,
        remote_actions: Arc::default(),
        tray: None,
        _display_power: None,
        _background_window: None,
    }
}

pub fn run_preview_client() -> Result<()> {
    let mut reader = std::io::stdin();
    let first = read_packet(&mut reader).context("Reading initial preview state")?;
    let runtime = remote_runtime(&first);
    let receiving = RemoteState::new(&runtime);
    apply_packet(&receiving, first);
    // Detached: joining stdin on GUI close would wait for the parent's pipe EOF.
    thread::spawn(move || {
        while let Ok(packet) = read_packet(&mut reader) {
            apply_packet(&receiving, packet);
        }
        receiving.shutdown.store(true, Ordering::Relaxed);
        if let Some(target) = receiving.repaint.lock().unwrap().clone() {
            target.ctx.request_repaint();
        }
    });
    run_preview(&runtime, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn packet(frame: bool) -> Packet {
        Packet {
            settings: Settings::default(),
            settings_path: "settings.json".into(),
            agents: AgentSnapshot::default(),
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
            error: String::new(),
            requests: vec![UiRequest::Settings],
            has_frame: frame,
            can_hide: true,
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
        assert!(matches!(decoded.requests[0], UiRequest::Settings));
    }

    #[test]
    fn metadata_without_frame_does_not_send_pixels() {
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &packet(false)).unwrap();
        assert!(bytes.len() < 16384);
        let decoded = read_packet(&mut Cursor::new(bytes)).unwrap();
        assert!(!decoded.has_frame);
        assert!(decoded.output.frame.is_none());
    }

    #[test]
    fn status_updates_preserve_the_last_frame_and_deliver_actions() {
        let state = RemoteState {
            agents: Arc::default(),
            view: Arc::default(),
            output: Arc::default(),
            actions: Arc::default(),
            shutdown: Arc::new(AtomicBool::new(false)),
            repaint: Arc::default(),
        };
        apply_packet(&state, packet(true));
        let frame = state.output.lock().unwrap().frame.clone().unwrap();
        let mut update = packet(false);
        update.output.status = "LCD reconnecting".into();
        apply_packet(&state, update);
        let output = state.output.lock().unwrap();
        assert_eq!(output.status, "LCD reconnecting");
        assert!(Arc::ptr_eq(output.frame.as_ref().unwrap(), &frame));
        assert_eq!(state.actions.lock().unwrap().len(), 2);
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
    fn commands_roundtrip() {
        let mut bytes = Vec::new();
        send_command(&mut bytes, &ClientCommand::Advance(-1)).unwrap();
        let command: ClientCommand = read_json(&mut Cursor::new(bytes), MAX_COMMAND).unwrap();
        assert!(matches!(command, ClientCommand::Advance(-1)));
    }

    #[test]
    fn decoded_commands_update_background_settings_view_and_shutdown() {
        let runtime = remote_runtime(&packet(false));
        let mut view = ViewState::default();
        view.preferences.mode = ViewMode::Overview;
        let settings = Settings {
            brightness: 7,
            rotate: false,
            ..Default::default()
        };
        for command in [
            ClientCommand::Settings(settings),
            ClientCommand::View(view),
            ClientCommand::Quit,
        ] {
            let mut bytes = Vec::new();
            send_command(&mut bytes, &command).unwrap();
            apply_command(
                &runtime,
                read_json(&mut Cursor::new(bytes), MAX_COMMAND).unwrap(),
            );
        }
        assert_eq!(runtime.shared_settings.lock().unwrap().brightness, 7);
        assert!(!runtime.shared_settings.lock().unwrap().rotate);
        assert_eq!(
            runtime.view.lock().unwrap().preferences.mode,
            ViewMode::Overview
        );
        assert!(runtime.shutdown.load(Ordering::Relaxed));
    }
}
