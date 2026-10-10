use crate::{
    APP_NAME, Args,
    agents::AgentKind,
    config::Settings,
    metrics::{Metrics, Snapshot},
    monitor::Monitor,
    render::Renderer,
    session::{AgentSnapshot, OpenState, ViewMode, ViewState},
    usb::Lcd,
};
use anyhow::{Context, Result};
use eframe::egui;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(windows)]
mod preview;
#[cfg(windows)]
pub use preview::run_preview_client;

pub struct InstanceGuard {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}
impl InstanceGuard {
    pub fn acquire() -> Result<Self> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::{
                Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError},
                System::Threading::CreateMutexW,
            };
            let name: Vec<u16> = format!("Local\\{APP_NAME}")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            // The handle stays alive for the process; Windows releases it on exit/crash.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            anyhow::ensure!(!handle.is_null(), "Cannot create single-instance mutex");
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe { CloseHandle(handle) };
                anyhow::bail!("{APP_NAME} is already running; open it from the system tray");
            }
            Ok(Self { handle })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {})
        }
    }
}
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Output {
    #[serde(skip)]
    frame: Option<Arc<image::RgbaImage>>,
    sequence: u64,
    connected: bool,
    status: String,
    fps: f64,
    night: bool,
    system_off: bool,
    view: ViewState,
}
impl Default for Output {
    fn default() -> Self {
        Self {
            frame: None,
            sequence: 0,
            connected: false,
            status: "Starting…".into(),
            fps: 0.0,
            night: false,
            system_off: false,
            view: ViewState::default(),
        }
    }
}
impl Output {
    fn preview_frame(&self, uploaded_sequence: Option<u64>) -> Option<Arc<image::RgbaImage>> {
        if uploaded_sequence == Some(self.sequence) {
            None
        } else {
            self.frame.clone()
        }
    }
}
struct Dashboard {
    settings: Settings,
    settings_path: PathBuf,
    shared_settings: Arc<Mutex<Settings>>,
    agents: Arc<Mutex<AgentSnapshot>>,
    view: Arc<Mutex<ViewState>>,
    frame_view: ViewState,
    show_sessions: bool,
    output: Arc<Mutex<Output>>,
    shutdown: Arc<AtomicBool>,
    repaint_target: RepaintTarget,
    can_hide: bool,
    #[cfg(windows)]
    requests: Arc<Mutex<Vec<preview::Request>>>,
    #[cfg(windows)]
    commands: std::io::Stdout,
    texture: Option<egui::TextureHandle>,
    sequence: u64,
    error: String,
    shared_error: Arc<Mutex<String>>,
    last_shared_error: String,
    show_settings: bool,
    quit: bool,
}

#[derive(Clone)]
struct RepaintWindow {
    ctx: egui::Context,
    #[cfg(windows)]
    window: NativeWindow,
}

#[cfg(windows)]
struct BackgroundWindow(windows_sys::Win32::Foundation::HWND);
#[cfg(windows)]
impl BackgroundWindow {
    fn new() -> Result<Self> {
        use windows_sys::{Win32::UI::WindowsAndMessaging::CreateWindowExW, core::w};
        // A hidden top-level window receives power broadcasts; a message-only
        // window would miss suspend/resume. It never creates a graphics context.
        let window = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                w!("Thermalright background"),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        anyhow::ensure!(
            !window.is_null(),
            "Creating background notification window failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(window))
    }
}
#[cfg(windows)]
impl Drop for BackgroundWindow {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
        }
    }
}
#[cfg(windows)]
fn pump_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
type RepaintTarget = Arc<Mutex<Option<RepaintWindow>>>;

#[derive(Clone)]
struct UiState {
    settings_path: PathBuf,
    shared_settings: Arc<Mutex<Settings>>,
    agents: Arc<Mutex<AgentSnapshot>>,
    view: Arc<Mutex<ViewState>>,
    output: Arc<Mutex<Output>>,
    shutdown: Arc<AtomicBool>,
    repaint_target: RepaintTarget,
    error: Arc<Mutex<String>>,
    #[cfg(windows)]
    settings_epoch: Arc<std::sync::atomic::AtomicU64>,
    can_hide: bool,
    #[cfg(windows)]
    requests: Arc<Mutex<Vec<preview::Request>>>,
}
struct Runtime {
    ui: UiState,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    #[cfg(windows)]
    tray: Option<Tray>,
    #[cfg(windows)]
    _display_power: Option<crate::power::DisplayPower>,
    #[cfg(windows)]
    _background_window: Option<BackgroundWindow>,
}
impl Runtime {
    fn new(args: &Args, settings: Settings, mut renderer: Renderer) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let output = Arc::new(Mutex::new(Output::default()));
        let shared_settings = Arc::new(Mutex::new(settings));
        let agents = Arc::new(Mutex::new(AgentSnapshot::default()));
        let view = Arc::new(Mutex::new(ViewState::default()));
        let repaint_target: RepaintTarget = Arc::new(Mutex::new(None));
        let display_state = Arc::new(crate::power::DisplayState::default());
        #[cfg(windows)]
        let (background_window, window_error) = match BackgroundWindow::new() {
            Ok(window) => (Some(window), String::new()),
            Err(error) => (None, format!("Display power sync unavailable: {error:#}")),
        };
        #[cfg(windows)]
        let (display_power, power_error) = if let Some(window) = &background_window {
            match crate::power::DisplayPower::new(window.0, display_state.clone()) {
                Ok(monitor) => (Some(monitor), String::new()),
                Err(error) => (None, format!("Display power sync unavailable: {error:#}")),
            }
        } else {
            (None, window_error)
        };
        let agent_worker = if args.demo {
            None
        } else {
            let agents = agents.clone();
            let stop = stop.clone();
            let shared_settings = shared_settings.clone();
            let home = crate::home_for(args);
            Some(thread::spawn(move || {
                let mut monitor =
                    Monitor::new(home, shared_settings.lock().unwrap().agents.clone());
                while !stop.load(Ordering::Relaxed) {
                    monitor.settings(shared_settings.lock().unwrap().agents.clone());
                    let value = monitor.tick(&stop);
                    *agents.lock().unwrap() = value;
                    interruptible_sleep(&stop, Duration::from_millis(100));
                }
            }))
        };
        let metric_worker = {
            let stop = stop.clone();
            let snapshot = snapshot.clone();
            let agents = agents.clone();
            let demo = args.demo;
            let cores = args.cores;
            let demo_sessions = args.demo_sessions as usize;
            thread::spawn(move || {
                let mut metrics = if demo { None } else { Some(Metrics::new()) };
                let start = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let value = if let Some(metrics) = &mut metrics {
                        metrics.collect()
                    } else {
                        Snapshot::demo_count(start.elapsed().as_secs_f64(), cores, demo_sessions)
                    };
                    if demo {
                        *agents.lock().unwrap() = value.agents.clone();
                    }
                    *snapshot.lock().unwrap() = value;
                    interruptible_sleep(&stop, Duration::from_millis(500));
                }
            })
        };
        let display_worker = {
            let stop = stop.clone();
            let output = output.clone();
            let shared_settings = shared_settings.clone();
            let repaint_target = repaint_target.clone();
            let agents = agents.clone();
            let view = view.clone();
            thread::spawn(move || {
                let mut lcd: Option<Lcd> = None;
                let mut retry = Instant::now();
                let start = Instant::now();
                let mut frames = 0;
                let mut fps_start = Instant::now();
                let mut generation = display_state.generation.load(Ordering::Relaxed);
                while !stop.load(Ordering::Relaxed) {
                    let tick = Instant::now();
                    let suspended = display_state.suspended.load(Ordering::Relaxed);
                    let current_generation = display_state.generation.load(Ordering::Relaxed);
                    if suspended || current_generation != generation {
                        lcd = None;
                        retry = tick;
                        generation = current_generation;
                        let mut state = output.lock().unwrap();
                        state.connected = false;
                        state.status = if suspended {
                            "System sleeping; LCD paused".into()
                        } else {
                            "System awake; reconnecting LCD".into()
                        };
                    }
                    if !suspended && lcd.is_none() && tick >= retry {
                        match Lcd::open() {
                            Ok(device) => {
                                let mut state = output.lock().unwrap();
                                state.connected = true;
                                state.status = format!(
                                    "LCD {}×{} · PM {}",
                                    device.info.width, device.info.height, device.info.pm
                                );
                                lcd = Some(device);
                            }
                            Err(e) => {
                                let mut state = output.lock().unwrap();
                                state.connected = false;
                                state.status = format!("{e:#}");
                            }
                        }
                        retry = tick + Duration::from_secs(3);
                    }
                    let settings = shared_settings.lock().unwrap().clone();
                    let mut snapshot = snapshot.lock().unwrap().clone();
                    snapshot.agents = agents.lock().unwrap().clone();
                    let night = settings.is_night();

                    let off =
                        settings.follow_system_display && display_state.off.load(Ordering::Relaxed);
                    let blank = night || off || suspended;
                    let current_view = {
                        let mut view = view.lock().unwrap();
                        view.sync(&snapshot.agents, &settings.agent_view, tick, blank);
                        view.clone()
                    };
                    let frame = renderer.render_view(
                        &snapshot,
                        &current_view,
                        start.elapsed().as_secs_f64(),
                    );
                    if let Some(device) = &mut lcd {
                        let black;
                        let send_frame = if blank {
                            black = image::RgbaImage::from_pixel(
                                1920,
                                480,
                                image::Rgba([0, 0, 0, 255]),
                            );
                            &black
                        } else {
                            &frame
                        };
                        if let Err(e) = device.send(send_frame, &settings) {
                            let mut state = output.lock().unwrap();
                            state.connected = false;
                            state.status = format!("USB disconnected: {e:#}");
                            lcd = None;
                            retry = Instant::now() + Duration::from_secs(3);
                        }
                    }
                    frames += 1;
                    {
                        let mut state = output.lock().unwrap();
                        state.frame = Some(Arc::new(frame));
                        state.view = current_view;
                        state.sequence += 1;
                        state.night = night;
                        state.system_off = off;
                        if fps_start.elapsed() >= Duration::from_secs(1) {
                            state.fps = frames as f64 / fps_start.elapsed().as_secs_f64();
                            frames = 0;
                            fps_start = Instant::now();
                        }
                    }
                    if let Some(target) = repaint_target.lock().unwrap().as_ref() {
                        target.ctx.request_repaint();
                    }
                    let interval = if blank {
                        Duration::from_secs(3)
                    } else if snapshot.animated() {
                        Duration::from_secs_f64(1.0 / 15.0)
                    } else {
                        Duration::from_millis(500)
                    };
                    interruptible_sleep_while(
                        &stop,
                        interval.saturating_sub(tick.elapsed()),
                        || {
                            display_state.generation.load(Ordering::Relaxed) == generation
                                && display_state.suspended.load(Ordering::Relaxed) == suspended
                                && shared_settings.lock().unwrap().follow_system_display
                                    == settings.follow_system_display
                                && (!settings.follow_system_display
                                    || display_state.off.load(Ordering::Relaxed) == off)
                        },
                    );
                }
            })
        };

        #[cfg(windows)]
        let (tray, tray_error) = match Tray::new() {
            Ok(tray) => (Some(tray), String::new()),
            Err(error) => (None, format!("Tray unavailable: {error:#}")),
        };
        #[cfg(windows)]
        let error = [tray_error, power_error]
            .into_iter()
            .filter(|e| !e.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        #[cfg(not(windows))]
        let error = String::new();
        #[cfg(windows)]
        if !error.is_empty() {
            preview::diagnostic(&args.config.clone().unwrap_or_else(Settings::path), &error);
        }
        Ok(Self {
            ui: UiState {
                settings_path: args.config.clone().unwrap_or_else(Settings::path),
                shared_settings,
                agents,
                view,
                output,
                shutdown,
                repaint_target,
                error: Arc::new(Mutex::new(error)),
                #[cfg(windows)]
                settings_epoch: Arc::default(),
                #[cfg(windows)]
                can_hide: tray.is_some(),
                #[cfg(not(windows))]
                can_hide: false,
                #[cfg(windows)]
                requests: Arc::default(),
            },
            stop,
            workers: [Some(metric_worker), Some(display_worker), agent_worker]
                .into_iter()
                .flatten()
                .collect(),
            #[cfg(windows)]
            tray,
            #[cfg(windows)]
            _display_power: display_power,
            #[cfg(windows)]
            _background_window: background_window,
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        *self.ui.repaint_target.lock().unwrap() = None;
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

pub fn run(args: Args, settings: Settings) -> Result<()> {
    let _instance = InstanceGuard::acquire()?;
    let renderer = Renderer::new(settings.font.as_deref())?;
    let runtime = Runtime::new(&args, settings, renderer)?;
    #[cfg(windows)]
    {
        let mut wanted = !args.background || args.preview || !runtime.ui.can_hide;
        let mut settings_wanted = false;
        let mut startup_decided = wanted;
        let (mut last_connected, mut last_night) = (false, false);
        let mut process: Option<preview::PreviewProcess> = None;
        let mut retry = preview::SpawnRetry::default();
        while !runtime.ui.shutdown.load(Ordering::Relaxed) {
            pump_messages();
            if let Some(child) = &mut process {
                child.apply_commands(&runtime.ui);
                if runtime.ui.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                match child.exited() {
                    Ok(true) => {
                        child.apply_commands(&runtime.ui);
                        let pending = child.pending_requests();
                        if !pending.is_empty() {
                            wanted = retry.failed(Instant::now());
                            settings_wanted |= pending
                                .iter()
                                .any(|request| matches!(request, preview::UiRequest::Settings));
                        }
                        process = None;
                        let output = runtime.ui.output.lock().unwrap();
                        last_connected = output.connected;
                        last_night = output.night;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        preview::report(
                            &runtime.ui,
                            &format!("Checking preview process failed: {error:#}"),
                        );
                        let pending = child.pending_requests();
                        if !pending.is_empty() {
                            wanted = retry.failed(Instant::now());
                            settings_wanted |= pending
                                .iter()
                                .any(|request| matches!(request, preview::UiRequest::Settings));
                        }
                        process = None;
                    }
                }
            }
            if runtime.ui.shutdown.load(Ordering::Relaxed) {
                break;
            }
            for action in runtime.tray.as_ref().map(Tray::actions).unwrap_or_default() {
                if matches!(action, TrayAction::Quit) {
                    runtime.ui.shutdown.store(true, Ordering::Relaxed);
                    break;
                }
                retry.reset();
                let request = match action {
                    TrayAction::Settings => preview::UiRequest::Settings,
                    TrayAction::Preview => preview::UiRequest::Preview,
                    TrayAction::Quit => unreachable!(),
                };
                if let Some(child) = &mut process {
                    child.request(request);
                } else {
                    wanted = true;
                    settings_wanted |= matches!(request, preview::UiRequest::Settings);
                }
            }
            if runtime.ui.shutdown.load(Ordering::Relaxed) {
                break;
            }
            {
                let output = runtime.ui.output.lock().unwrap();
                if !startup_decided && output.sequence > 0 {
                    startup_decided = true;
                    wanted |= !output.connected;
                }
                wanted |= (last_connected && !output.connected) || (!last_night && output.night);
                last_connected = output.connected;
                last_night = output.night;
            }
            if wanted && let Some(child) = &mut process {
                child.request(preview::UiRequest::Preview);
                wanted = false;
            }
            if wanted && retry.ready(Instant::now()) {
                match preview::PreviewProcess::spawn(&runtime, settings_wanted) {
                    Ok(child) => {
                        process = Some(child);
                        wanted = false;
                        settings_wanted = false;
                    }
                    Err(error) => {
                        preview::report(
                            &runtime.ui,
                            &format!("Starting preview failed: {error:#}"),
                        );
                        if !retry.failed(Instant::now()) {
                            wanted = false;
                        }
                    }
                }
                startup_decided = true;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
    #[cfg(not(windows))]
    run_preview(&runtime.ui, false)?;
    Ok(())
}

fn run_preview(runtime: &UiState, show_settings: bool) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1280.0, 400.0])
            .with_min_inner_size([800.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            let settings = runtime.shared_settings.lock().unwrap().clone();
            let default_font = crate::fonts::default_path();
            if let Ok(bytes) =
                crate::fonts::bytes(settings.font.as_deref().unwrap_or(&default_font))
            {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "chinese".into(),
                    Arc::new(egui::FontData::from_static(bytes)),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "chinese".into());
                cc.egui_ctx.set_fonts(fonts);
            }
            let dashboard = Dashboard::new(runtime, settings, show_settings);
            *runtime.repaint_target.lock().unwrap() = Some(RepaintWindow {
                ctx: cc.egui_ctx.clone(),
                #[cfg(windows)]
                window: NativeWindow::from_context(cc)?,
            });
            Ok(Box::new(dashboard))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Window error: {error}"))
}

impl Dashboard {
    fn new(runtime: &UiState, settings: Settings, show_settings: bool) -> Self {
        let error = runtime.error.lock().unwrap().clone();
        Self {
            settings,
            settings_path: runtime.settings_path.clone(),
            shared_settings: runtime.shared_settings.clone(),
            agents: runtime.agents.clone(),
            view: runtime.view.clone(),
            output: runtime.output.clone(),
            shutdown: runtime.shutdown.clone(),
            repaint_target: runtime.repaint_target.clone(),
            can_hide: runtime.can_hide,
            #[cfg(windows)]
            requests: runtime.requests.clone(),
            #[cfg(windows)]
            commands: std::io::stdout(),
            frame_view: ViewState::default(),
            show_sessions: false,
            texture: None,
            sequence: 0,
            error: error.clone(),
            shared_error: runtime.error.clone(),
            last_shared_error: error,
            show_settings,
            quit: false,
        }
    }
    fn close_preview(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    fn can_hide(&self) -> bool {
        self.can_hide
    }
    fn save(&mut self) {
        *self.shared_settings.lock().unwrap() = self.settings.clone();
        #[cfg(windows)]
        self.command(preview::ClientCommand::Settings(self.settings.clone()));
        #[cfg(not(windows))]
        if let Err(e) = self.settings.save(&self.settings_path) {
            self.error = e.to_string();
        }
    }
    #[cfg(windows)]
    fn command(&mut self, command: preview::ClientCommand) {
        if let Err(error) = preview::send_command(&mut self.commands, &command) {
            self.error = format!("后台连接已断开：{error}");
            self.shutdown.store(true, Ordering::Relaxed);
        }
    }
    fn settings_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut changed = false;
        egui::Window::new("设置")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                changed |= ui
                    .checkbox(
                        &mut self.settings.agents.windows_enabled,
                        "采集 Windows 当前用户",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.settings.agents.wsl_running_enabled,
                        "发现运行中的 WSL2",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.settings.agents.wsl_default_user,
                        "采集 WSL 默认用户",
                    )
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut self.settings.brightness, 1..=10).text("亮度"))
                    .changed();
                changed |= ui
                    .checkbox(&mut self.settings.rotate, "LCD 旋转 180°")
                    .changed();
                #[cfg(windows)]
                {
                    changed |= ui
                        .checkbox(
                            &mut self.settings.follow_system_display,
                            "跟随系统熄屏/亮屏",
                        )
                        .changed();
                }
                changed |= ui
                    .checkbox(
                        &mut self.settings.night_enabled,
                        "夜间熄屏（本机预览继续运行）",
                    )
                    .changed();
                ui.horizontal(|ui| {
                    ui.label("熄屏时间");
                    changed |= minute_editor(ui, &mut self.settings.night_start);
                    ui.label("到");
                    changed |= minute_editor(ui, &mut self.settings.night_end);
                });
                #[cfg(windows)]
                {
                    let mut enabled = autostart_enabled();
                    if ui
                        .checkbox(&mut enabled, "登录 Windows 后启动到托盘")
                        .changed()
                        && let Err(e) = set_autostart(enabled)
                    {
                        self.error = e.to_string();
                    }
                }
                ui.separator();
                ui.label("CPU 温度：运行 LibreHardwareMonitor 并启用 WMI。");
                ui.label("额度来自 Codex 本地日志；Cursor 不提供 Token 用量。");
                ui.label(format!("配置文件：{}", self.settings_path.display()));
            });
        self.show_settings = open;
        if changed {
            self.save();
        }
    }
}
impl eframe::App for Dashboard {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.quit || self.shutdown.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        #[cfg(windows)]
        {
            let requests: Vec<_> = self.requests.lock().unwrap().drain(..).collect();
            for request in requests {
                match request.action {
                    preview::UiRequest::Preview => {}
                    preview::UiRequest::Settings => self.show_settings = true,
                }
                if let Some(target) = self.repaint_target.lock().unwrap().clone() {
                    target.window.restore(true);
                }
                self.command(preview::ClientCommand::HandledRequest(request.id));
            }
        }
        let shared_error = self.shared_error.lock().unwrap().clone();
        if shared_error != self.last_shared_error {
            self.error = shared_error.clone();
            self.last_shared_error = shared_error;
        }
        self.settings = self.shared_settings.lock().unwrap().clone();
        let (status, fps, night, system_off, sequence, frame, frame_view) = {
            let output = self.output.lock().unwrap();
            (
                output.status.clone(),
                output.fps,
                output.night,
                output.system_off,
                output.sequence,
                output.preview_frame(self.texture.as_ref().map(|_| self.sequence)),
                output.view.clone(),
            )
        };
        if let Some(frame) = frame {
            self.frame_view = frame_view;
            let pixels = egui::ColorImage::from_rgba_unmultiplied([1920, 480], frame.as_raw());
            if let Some(texture) = &mut self.texture {
                texture.set(pixels, egui::TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(ctx.load_texture("dashboard", pixels, egui::TextureOptions::LINEAR));
            }
            self.sequence = sequence;
        }
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.strong(APP_NAME);
                if ui.button("设置").clicked() {
                    self.show_settings = true;
                }
                if self.can_hide() && ui.button("隐藏到托盘").clicked() {
                    self.close_preview(ctx);
                }
                if ui.button("退出").clicked() {
                    self.quit = true;
                }
                ui.label(format!(
                    "{fps:.1} fps{}",
                    if system_off {
                        " · 跟随系统熄屏"
                    } else if night {
                        " · 夜间熄屏"
                    } else {
                        ""
                    }
                ));
            });
        });
        egui::TopBottomPanel::top("agent-controls").show(ctx, |ui| {
            let mut changed = false;
            ui.horizontal_wrapped(|ui| {
                for (mode, label) in [
                    (ViewMode::Auto, "自动布局"),
                    (ViewMode::Overview, "总览"),
                    (ViewMode::Details, "详情"),
                ] {
                    changed |= ui
                        .selectable_value(&mut self.settings.agent_view.mode, mode, label)
                        .changed();
                }
                egui::ComboBox::from_id_salt("agent-filter")
                    .selected_text(format!("Agent: {}", self.settings.agent_view.agent_filter))
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(
                                &mut self.settings.agent_view.agent_filter,
                                "all".into(),
                                "全部 Agent",
                            )
                            .changed();
                        for kind in AgentKind::ALL {
                            changed |= ui
                                .selectable_value(
                                    &mut self.settings.agent_view.agent_filter,
                                    kind.name().to_lowercase(),
                                    kind.name(),
                                )
                                .changed();
                        }
                    });
                let agents = self.agents.lock().unwrap().clone();
                let origin_name = agents
                    .origins
                    .iter()
                    .find(|o| o.origin_id == self.settings.agent_view.origin_filter)
                    .map(|o| o.display_name.as_str())
                    .unwrap_or("全部来源");
                egui::ComboBox::from_id_salt("origin-filter")
                    .selected_text(origin_name)
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(
                                &mut self.settings.agent_view.origin_filter,
                                "all".into(),
                                "全部来源",
                            )
                            .changed();
                        for origin in &agents.origins {
                            changed |= ui
                                .selectable_value(
                                    &mut self.settings.agent_view.origin_filter,
                                    origin.origin_id.clone(),
                                    &origin.display_name,
                                )
                                .changed();
                        }
                    });
                if self.settings.agent_view.origin_filter != "all"
                    && agents.origins_ready
                    && !agents.origins.iter().any(|o| {
                        o.origin_id == self.settings.agent_view.origin_filter
                            && o.status != "已停止"
                    })
                {
                    self.settings.agent_view.origin_filter = "all".into();
                    self.error = "保存的来源已不存在，已恢复全部来源。".into();
                    changed = true;
                }
                for (delta, label) in [(-1, "上一页"), (1, "下一页")] {
                    if ui.button(label).clicked() {
                        self.view.lock().unwrap().advance(delta, Instant::now());
                        #[cfg(windows)]
                        self.command(preview::ClientCommand::Advance(delta));
                    }
                }
                changed |= ui
                    .checkbox(
                        &mut self.settings.agent_view.auto_rotate,
                        format!(
                            "每 {} 秒轮播",
                            self.settings.agent_view.rotate_interval_seconds
                        ),
                    )
                    .changed();
                if ui.button("会话与来源状态").clicked() {
                    self.show_sessions = !self.show_sessions;
                }
            });
            if changed {
                self.save();
            }
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.label(status);
            if !self.error.is_empty() {
                ui.colored_label(egui::Color32::LIGHT_RED, &self.error);
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(texture) = &self.texture {
                let available = ui.available_size();
                let width = available.x.min(available.y * 4.0);
                let response = ui.add(
                    egui::Image::new(texture)
                        .fit_to_exact_size(egui::vec2(width, width / 4.0))
                        .sense(egui::Sense::click()),
                );
                if response.clicked()
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    let x = (pos.x - response.rect.min.x) * 1920.0 / response.rect.width();
                    let y = (pos.y - response.rect.min.y) * 480.0 / response.rect.height();
                    let mut view = self.frame_view.clone();
                    if view.click(x, y, Instant::now()) {
                        self.settings.agent_view = view.preferences.clone();
                        *self.view.lock().unwrap() = view.clone();
                        self.save();
                        #[cfg(windows)]
                        self.command(preview::ClientCommand::View(view));
                    }
                }
            } else {
                ui.spinner();
            }
        });
        if self.show_sessions {
            let agents = self.agents.lock().unwrap().clone();
            egui::Window::new("会话与来源状态").open(&mut self.show_sessions).default_width(650.0).show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(500.0).show(ui, |ui| {
                    for origin in &agents.origins {
                        ui.collapsing(format!("{} · {}", origin.display_name, origin.status), |ui| {
                            ui.label(format!("来源：{}", origin.origin_id));
                            ui.label(format!("用户：{} · 最后成功：{}", origin.user.as_deref().unwrap_or("未知"), display_time(origin.last_success)));
                            if let Some(error) = &origin.error { ui.colored_label(egui::Color32::YELLOW, error); }
                        });
                    }
                    ui.separator();
                    for session in &agents.sessions {
                        ui.collapsing(format!("{} · {} · {}", session.key.agent_kind.name(), session.usage.project, session.label()), |ui| {
                            ui.label(format!("会话：{}", session.key.native_session_id));
                            ui.label(format!("来源：{}", session.key.origin_id));
                            ui.label(format!("项目路径：{}", session.usage.project_path));
                            ui.label(format!("日志：{:?}", session.usage.log_path));
                            ui.label(format!("证据：{} · 实例数 {} · 最后确认 {}", session.evidence, session.instance_count, display_time(session.last_verified_at)));
                            for instance in agents.instances.iter().filter(|i| i.has_session(&session.key)) { ui.label(format!("PID {} · 创建时间 {}", instance.instance_key.pid, instance.instance_key.process_started_at)); }
                            if let Some(used) = session.usage.quota_used { ui.label(format!("来源范围额度日志读数：已用 {used:.0}% · 账户归属未知")); }
                            if session.open_state == OpenState::Unconfirmed { ui.label("运行实例证据尚未确认"); }
                            ui.label(&session.usage.message);
                        });
                    }
                });
            });
        }
        if self.show_settings {
            self.settings_ui(ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}
impl Drop for Dashboard {
    fn drop(&mut self) {
        *self.repaint_target.lock().unwrap() = None;
        if self.quit || !self.can_hide() {
            #[cfg(windows)]
            self.command(preview::ClientCommand::Quit);
            self.shutdown.store(true, Ordering::Relaxed);
        }
    }
}
fn interruptible_sleep(stop: &AtomicBool, duration: Duration) {
    interruptible_sleep_while(stop, duration, || true);
}
fn interruptible_sleep_while(
    stop: &AtomicBool,
    duration: Duration,
    keep_sleeping: impl Fn() -> bool,
) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) && keep_sleeping() && Instant::now() < deadline {
        thread::sleep(
            Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}
fn minute_editor(ui: &mut egui::Ui, minute: &mut u16) -> bool {
    let mut hour = *minute / 60;
    let mut min = *minute % 60;
    let changed = ui
        .add(egui::DragValue::new(&mut hour).range(0..=23))
        .changed()
        | ui.add(egui::DragValue::new(&mut min).range(0..=59))
            .changed();
    *minute = hour * 60 + min;
    changed
}

#[cfg(windows)]
enum TrayAction {
    Preview,
    Settings,
    Quit,
}
#[cfg(windows)]
struct Tray {
    _icon: tray_icon::TrayIcon,
    pending: Arc<Mutex<Vec<TrayAction>>>,
}
#[cfg(windows)]
impl Tray {
    fn new() -> Result<Self> {
        use tray_icon::{
            Icon, TrayIconBuilder, TrayIconEvent,
            menu::{Menu, MenuEvent, MenuItem},
        };
        let menu = Menu::new();
        let preview = MenuItem::new("打开预览", true, None);
        let settings = MenuItem::new("设置", true, None);
        let quit = MenuItem::new("退出", true, None);
        menu.append_items(&[&preview, &settings, &quit])?;
        let pending = Arc::new(Mutex::new(Vec::new()));
        let preview_id = preview.id().clone();
        let settings_id = settings.id().clone();
        let quit_id = quit.id().clone();
        // The tray belongs to the background runtime. With no preview, the outer
        // message pump consumes these actions; an existing preview is restored.
        // Install handlers before creating the icon: they are initialized once.
        let events = pending.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = if event.id == preview_id {
                TrayAction::Preview
            } else if event.id == settings_id {
                TrayAction::Settings
            } else if event.id == quit_id {
                TrayAction::Quit
            } else {
                return;
            };
            events.lock().unwrap().push(action);
        }));
        let events = pending.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                events.lock().unwrap().push(TrayAction::Preview);
            }
        }));
        let mut pixels = vec![0; 32 * 32 * 4];
        for y in 3..29 {
            for x in 3..29 {
                let i = (y * 32 + x) * 4;
                pixels[i..i + 4].copy_from_slice(&if !(6..=25).contains(&x)
                    || !(6..=25).contains(&y)
                {
                    [52, 211, 153, 255]
                } else {
                    [20, 23, 34, 255]
                });
            }
        }
        let icon = TrayIconBuilder::new()
            .with_tooltip(APP_NAME)
            .with_icon(Icon::from_rgba(pixels, 32, 32)?)
            .with_menu(Box::new(menu))
            .build()?;
        Ok(Self {
            _icon: icon,
            pending,
        })
    }
    fn actions(&self) -> Vec<TrayAction> {
        self.pending.lock().unwrap().drain(..).collect()
    }
}
#[cfg(windows)]
#[derive(Clone, Copy)]
struct NativeWindow(usize);

#[cfg(windows)]
impl NativeWindow {
    fn from_context(cc: &eframe::CreationContext<'_>) -> Result<Self> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match cc.window_handle()?.as_raw() {
            RawWindowHandle::Win32(handle) => Ok(Self(handle.hwnd.get() as usize)),
            _ => anyhow::bail!("Expected a Win32 preview window"),
        }
    }
    fn restore(self, focus: bool) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            IsIconic, IsWindow, SW_RESTORE, SW_SHOW, SW_SHOWNOACTIVATE, SetForegroundWindow,
            ShowWindow,
        };
        // Tray handlers run on the window's event-loop thread. The HWND belongs
        // to the root viewport and is used only while that viewport is alive.
        let hwnd = self.0 as windows_sys::Win32::Foundation::HWND;
        unsafe {
            if IsWindow(hwnd) == 0 {
                return;
            }
            let show = if IsIconic(hwnd) != 0 {
                SW_RESTORE
            } else if focus {
                SW_SHOW
            } else {
                SW_SHOWNOACTIVATE
            };
            ShowWindow(hwnd, show);
            if focus {
                SetForegroundWindow(hwnd);
            }
        }
    }
}

#[cfg(windows)]
fn autostart_enabled() -> bool {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .ok()
        .and_then(|key| key.get_value::<String, _>(APP_NAME).ok())
        .is_some()
}
#[cfg(windows)]
fn set_autostart(enabled: bool) -> Result<()> {
    use winreg::{
        RegKey,
        enums::{HKEY_CURRENT_USER, KEY_SET_VALUE},
    };
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
        KEY_SET_VALUE,
    )?;
    if enabled {
        let exe = std::env::current_exe().context("Finding executable for autostart")?;
        key.set_value(APP_NAME, &format!("\"{}\" --background", exe.display()))?;
    } else if let Err(e) = key.delete_value(APP_NAME)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e.into());
    }
    Ok(())
}

fn display_time(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| {
            format!(
                "{}（{} 秒前）",
                t.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S"),
                (chrono::Utc::now().timestamp() - t.timestamp()).max(0)
            )
        })
        .unwrap_or_else(|| "尚未确认".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_uploads_each_sequence_once_and_reopens_with_latest_frame() {
        let first = Arc::new(image::RgbaImage::new(2, 1));
        let mut output = Output {
            frame: Some(first.clone()),
            sequence: 1,
            ..Default::default()
        };
        assert!(output.preview_frame(Some(1)).is_none());
        let frame = output.preview_frame(None).unwrap();
        assert!(Arc::ptr_eq(&frame, &first));
        assert!(output.preview_frame(Some(1)).is_none());
        let latest = Arc::new(image::RgbaImage::new(2, 1));
        output.frame = Some(latest.clone());
        output.sequence = 2;
        assert!(output.preview_frame(Some(2)).is_none());
        assert!(Arc::ptr_eq(
            &output.preview_frame(Some(1)).unwrap(),
            &latest
        ));
        // Reopening after releasing the texture must upload even an unchanged frame.
        assert!(Arc::ptr_eq(&output.preview_frame(None).unwrap(), &latest));
    }

    #[test]
    fn display_power_changes_interrupt_the_frame_interval() {
        for (initially_off, resume) in [(false, false), (true, false), (true, true)] {
            let state = Arc::new(crate::power::DisplayState::default());
            state.off.store(initially_off, Ordering::Relaxed);
            let worker_state = state.clone();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (finished_tx, finished_rx) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                let stop = AtomicBool::new(false);
                started_tx.send(()).unwrap();
                interruptible_sleep_while(&stop, Duration::from_secs(3), || {
                    worker_state.off.load(Ordering::Relaxed) == initially_off
                        && worker_state.generation.load(Ordering::Relaxed) == 0
                });
                finished_tx.send(()).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            if resume {
                state.generation.fetch_add(1, Ordering::Relaxed);
            } else {
                state.off.store(!initially_off, Ordering::Relaxed);
            }
            let result = finished_rx.recv_timeout(Duration::from_millis(500));
            worker.join().unwrap();
            result.expect("LCD should respond without waiting for the three-second black interval");
        }
    }
}
