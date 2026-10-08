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

struct Output {
    frame: Option<image::RgbaImage>,
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
struct Dashboard {
    settings: Settings,
    settings_path: PathBuf,
    shared_settings: Arc<Mutex<Settings>>,
    agents: Arc<Mutex<AgentSnapshot>>,
    view: Arc<Mutex<ViewState>>,
    frame_view: ViewState,
    show_sessions: bool,
    output: Arc<Mutex<Output>>,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    texture: Option<egui::TextureHandle>,
    sequence: u64,
    error: String,
    show_settings: bool,
    hidden: bool,
    force_preview: bool,
    startup_decided: bool,
    last_connected: bool,
    last_night: bool,
    quit: bool,
    #[cfg(windows)]
    tray: Option<Tray>,
    #[cfg(windows)]
    _display_power: Option<crate::power::DisplayPower>,
}

pub fn run(args: Args, settings: Settings) -> Result<()> {
    let _instance = InstanceGuard::acquire()?;
    // Fail before starting workers if the required font/art assets cannot be loaded.
    let renderer = Renderer::new(settings.font.as_deref())?;
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
            let default_font = std::env::var_os("WINDIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| "C:/Windows".into())
                .join("Fonts/msyh.ttc");
            if let Ok(bytes) = std::fs::read(settings.font.as_deref().unwrap_or(&default_font)) {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "chinese".into(),
                    Arc::new(egui::FontData::from_owned(bytes)),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "chinese".into());
                cc.egui_ctx.set_fonts(fonts);
            }
            Ok(Box::new(Dashboard::new(args, settings, renderer, cc)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("Window error: {e}"))
}

impl Dashboard {
    fn new(
        args: Args,
        settings: Settings,
        mut renderer: Renderer,
        cc: &eframe::CreationContext<'_>,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let output = Arc::new(Mutex::new(Output::default()));
        let shared_settings = Arc::new(Mutex::new(settings.clone()));
        let agents = Arc::new(Mutex::new(AgentSnapshot::default()));
        let view = Arc::new(Mutex::new(ViewState::default()));
        let system_off = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        let (display_power, power_error) = match NativeWindow::from_context(cc)
            .and_then(|window| crate::power::DisplayPower::new(window.0 as _, system_off.clone()))
        {
            Ok(monitor) => (Some(monitor), String::new()),
            Err(e) => (None, format!("Display power sync unavailable: {e:#}")),
        };
        let home = args.agent_home.clone().unwrap_or_else(|| {
            directories::UserDirs::new()
                .map(|d| d.home_dir().to_path_buf())
                .unwrap_or_default()
        });
        let agent_worker = if args.demo {
            None
        } else {
            let agents = agents.clone();
            let stop = stop.clone();
            let shared_settings = shared_settings.clone();
            let home = home.clone();
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
                let mut metrics = if demo { None } else { Some(Metrics::new(home)) };
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
            let ctx = ctx.clone();
            let agents = agents.clone();
            let view = view.clone();
            thread::spawn(move || {
                let mut lcd: Option<Lcd> = None;
                let mut retry = Instant::now();
                let start = Instant::now();
                let mut frames = 0;
                let mut fps_start = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let tick = Instant::now();
                    if lcd.is_none() && tick >= retry {
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
                                state.status = e.to_string();
                            }
                        }
                        retry = tick + Duration::from_secs(3);
                    }
                    let settings = shared_settings.lock().unwrap().clone();
                    let mut snapshot = snapshot.lock().unwrap().clone();
                    snapshot.agents = agents.lock().unwrap().clone();
                    let night = settings.is_night();

                    let off = settings.follow_system_display && system_off.load(Ordering::Relaxed);
                    let blank = night || off;
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
                            state.status = format!("USB disconnected: {e}");
                            lcd = None;
                            retry = Instant::now() + Duration::from_secs(3);
                        }
                    }
                    frames += 1;
                    {
                        let mut state = output.lock().unwrap();
                        state.frame = Some(frame);
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
                    ctx.request_repaint();
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
                            shared_settings.lock().unwrap().follow_system_display
                                == settings.follow_system_display
                                && (!settings.follow_system_display
                                    || system_off.load(Ordering::Relaxed) == off)
                        },
                    );
                }
            })
        };
        #[cfg(windows)]
        let (tray, tray_error) = match Tray::new(&ctx, cc) {
            Ok(t) => (Some(t), String::new()),
            Err(e) => (None, format!("Tray unavailable: {e}")),
        };
        Self {
            settings,
            settings_path: args.config.unwrap_or_else(Settings::path),
            shared_settings,
            agents,
            view,
            frame_view: ViewState::default(),
            show_sessions: false,
            output,
            stop,
            workers: [Some(metric_worker), Some(display_worker), agent_worker]
                .into_iter()
                .flatten()
                .collect(),
            texture: None,
            sequence: 0,
            #[cfg(windows)]
            error: [tray_error, power_error]
                .into_iter()
                .filter(|error| !error.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            #[cfg(not(windows))]
            error: String::new(),
            show_settings: false,
            hidden: false,
            force_preview: args.preview,
            startup_decided: !args.background,
            last_connected: false,
            last_night: false,
            quit: false,
            #[cfg(windows)]
            tray,
            #[cfg(windows)]
            _display_power: display_power,
        }
    }
    fn visible(&mut self, ctx: &egui::Context, visible: bool) {
        self.hidden = !visible;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
        if visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }
    fn can_hide(&self) -> bool {
        #[cfg(windows)]
        {
            self.tray.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
    fn save(&mut self) {
        *self.shared_settings.lock().unwrap() = self.settings.clone();
        if let Err(e) = self.settings.save(&self.settings_path) {
            self.error = e.to_string();
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
        #[cfg(windows)]
        {
            let actions = self.tray.as_ref().map(|t| t.actions()).unwrap_or_default();
            for action in actions {
                match action {
                    TrayAction::Preview => self.visible(ctx, true),
                    TrayAction::Settings => {
                        self.show_settings = true;
                        self.visible(ctx, true);
                    }
                    TrayAction::Quit => self.quit = true,
                }
            }
        }
        if self.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.can_hide() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.visible(ctx, false);
        }
        let (connected, status, fps, night, system_off, sequence, frame, frame_view) = {
            let output = self.output.lock().unwrap();
            (
                output.connected,
                output.status.clone(),
                output.fps,
                output.night,
                output.system_off,
                output.sequence,
                if output.sequence != self.sequence {
                    output.frame.clone()
                } else {
                    None
                },
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
        if !self.startup_decided && sequence > 0 {
            self.startup_decided = true;
            if connected && !self.force_preview && self.can_hide() {
                self.visible(ctx, false);
            }
        }
        if self.hidden && ((!connected && self.last_connected) || (night && !self.last_night)) {
            self.visible(ctx, true);
        }
        self.last_connected = connected;
        self.last_night = night;
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.strong(APP_NAME);
                if ui.button("设置").clicked() {
                    self.show_settings = true;
                }
                if self.can_hide() && ui.button("隐藏到托盘").clicked() {
                    self.visible(ctx, false);
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
                        *self.view.lock().unwrap() = view;
                        self.save();
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
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
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
    fn new(ctx: &egui::Context, cc: &eframe::CreationContext<'_>) -> Result<Self> {
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
        let window = NativeWindow::from_context(cc)?;
        let preview_id = preview.id().clone();
        let settings_id = settings.id().clone();
        let quit_id = quit.id().clone();
        // Hidden Win32 windows do not receive the redraw needed to run App::update.
        // Restore the HWND in the native tray callback before asking egui to repaint.
        // Install handlers before creating the icon, since these libraries' handlers
        // are initialized once, including when their first event is delivered.
        let wake = ctx.clone();
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
            let focus = !matches!(action, TrayAction::Quit);
            events.lock().unwrap().push(action);
            window.restore(focus);
            wake.request_repaint();
        }));
        let wake = ctx.clone();
        let events = pending.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                events.lock().unwrap().push(TrayAction::Preview);
                window.restore(true);
                wake.request_repaint();
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
    fn display_power_changes_interrupt_the_frame_interval() {
        for initially_off in [false, true] {
            let off = Arc::new(AtomicBool::new(initially_off));
            let worker_off = off.clone();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (finished_tx, finished_rx) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                let stop = AtomicBool::new(false);
                started_tx.send(()).unwrap();
                interruptible_sleep_while(&stop, Duration::from_secs(3), || {
                    worker_off.load(Ordering::Relaxed) == initially_off
                });
                finished_tx.send(()).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            off.store(!initially_off, Ordering::Relaxed);
            let result = finished_rx.recv_timeout(Duration::from_millis(500));
            worker.join().unwrap();
            result.expect("LCD should respond without waiting for the three-second black interval");
        }
    }
}
