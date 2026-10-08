#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agents;
mod app;
mod config;
mod metrics;
mod monitor;
#[cfg(windows)]
mod power;
mod probe;
mod protocol;
mod render;
mod session;
mod usb;

use anyhow::{Context, Result};
use clap::Parser;
use std::{path::PathBuf, time::Instant};

pub const APP_NAME: &str = env!("CARGO_PKG_NAME");

#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
    /// Always show the preview window (USB output remains active).
    #[arg(long)]
    preview: bool,
    /// Use deterministic sample data instead of reading local sessions.
    #[arg(long)]
    demo: bool,
    /// Render a demo frame to PNG without opening USB or a window.
    #[arg(long)]
    snapshot: Option<PathBuf>,
    /// Export an animated demo GIF.
    #[arg(long)]
    gif: Option<PathBuf>,
    #[arg(long, default_value_t = 48, value_parser = clap::value_parser!(u32).range(1..=1200))]
    frames: u32,
    #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: u32,
    /// GIF downsampling divisor.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=8))]
    scale: u32,
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..=256))]
    cores: u32,
    /// Number of sessions in deterministic demo data.
    #[arg(long, default_value_t = 9, value_parser = clap::value_parser!(u32).range(0..=256))]
    demo_sessions: u32,
    /// Zero-based page for PNG/GIF demo export.
    #[arg(long, default_value_t = 0)]
    demo_page: usize,
    /// Override the configured session layout for this run.
    #[arg(long, value_enum)]
    agent_mode: Option<session::ViewMode>,
    /// Send this many demo frames and report actual USB throughput.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=10000))]
    benchmark: Option<u32>,
    /// Override the persisted settings file.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Read agent logs from a different home directory.
    #[arg(long)]
    agent_home: Option<PathBuf>,
    /// Run hidden in the system tray until disconnected or opened.
    #[arg(long)]
    background: bool,
    /// Print local system/agent availability without opening a window or USB.
    #[arg(long)]
    diagnostics: bool,
    /// Keep collecting for this many seconds before printing diagnostics.
    #[arg(long, default_value_t = 0, requires = "diagnostics", value_parser = clap::value_parser!(u32).range(0..=300))]
    diagnostics_seconds: u32,
    #[arg(long, hide = true)]
    probe_windows: bool,
}

fn main() {
    #[cfg(all(windows, not(debug_assertions)))]
    let headless = std::env::args().any(|arg| {
        matches!(
            arg.as_str(),
            "--snapshot" | "--gif" | "--benchmark" | "--diagnostics" | "--probe-windows"
        )
    });
    #[cfg(all(windows, not(debug_assertions)))]
    let console = attach_parent_console();
    if let Err(error) = execute() {
        eprintln!("{error:#}");
        #[cfg(all(windows, not(debug_assertions)))]
        if !console && !headless {
            let title: Vec<u16> = APP_NAME.encode_utf16().chain(Some(0)).collect();
            let message: Vec<u16> = format!("{error:#}").encode_utf16().chain(Some(0)).collect();
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    std::ptr::null_mut(),
                    message.as_ptr(),
                    title.as_ptr(),
                    windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                        | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}

#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console() -> bool {
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType},
        System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        },
    };
    // AttachConsole replaces inherited standard handles. Preserve the JSON
    // pipes of probes and redirected diagnostics before considering it.
    let redirected = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        .any(|id| unsafe {
            matches!(
                GetFileType(GetStdHandle(id)),
                FILE_TYPE_DISK | FILE_TYPE_PIPE
            )
        });
    !redirected && unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
}

fn execute() -> Result<()> {
    let args = Args::parse();
    if args.probe_windows {
        #[cfg(windows)]
        println!("{}", serde_json::to_string(&probe::windows_probe()?)?);
        return Ok(());
    }
    let mut cfg = config::Settings::load(args.config.as_deref())?;
    if let Some(mode) = args.agent_mode {
        cfg.agent_view.mode = mode;
    }
    if args.diagnostics {
        let home = args.agent_home.clone().unwrap_or_else(|| {
            directories::UserDirs::new()
                .map(|d| d.home_dir().to_path_buf())
                .unwrap_or_default()
        });
        let mut collector = metrics::Metrics::new(home);
        collector.collect();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let s = collector.collect();
        let mut monitor = monitor::Monitor::new(home_for(&args), cfg.agents.clone());
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut agents = monitor.collect_once(&stop);
        let deadline =
            Instant::now() + std::time::Duration::from_secs(args.diagnostics_seconds as u64);
        while Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(40));
            agents = monitor.tick(&stop);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"cpu":s.cpu_name,"cpu_percent":s.cpu,"logical_cores":s.cores.len(),"memory_total":s.total_memory,"memory_used":s.used_memory,"cpu_temperature":s.temperature,"processes":s.processes,"uptime_seconds":s.uptime,"agents":agents})
            )?
        );
        return Ok(());
    }
    if args.snapshot.is_some() || args.gif.is_some() || args.benchmark.is_some() {
        let mut renderer = render::Renderer::new(cfg.font.as_deref())?;
        if let Some(path) = &args.snapshot {
            demo_frame(&mut renderer, &args, &cfg, 0.0)
                .save(path)
                .with_context(|| format!("Saving {}", path.display()))?;
            println!("Saved {}", path.display());
        }
        if let Some(path) = &args.gif {
            use image::codecs::gif::{GifEncoder, Repeat};
            let mut encoder = GifEncoder::new(std::fs::File::create(path)?);
            encoder.set_repeat(Repeat::Infinite)?;
            for n in 0..args.frames {
                let t = n as f64 / args.fps as f64;
                let frame = demo_frame(&mut renderer, &args, &cfg, t);
                let frame = image::imageops::resize(
                    &frame,
                    1920 / args.scale,
                    480 / args.scale,
                    image::imageops::FilterType::Triangle,
                );
                encoder.encode_frame(image::Frame::from_parts(
                    frame,
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(1000, args.fps),
                ))?;
            }
            println!("Saved {}", path.display());
        }
        if let Some(count) = args.benchmark {
            let _instance = app::InstanceGuard::acquire()?;
            let mut lcd = usb::Lcd::open()?;
            println!(
                "LCD {}×{} · PM {}",
                lcd.info.width, lcd.info.height, lcd.info.pm
            );
            let start = Instant::now();
            for n in 0..count {
                let t = n as f64 / 15.0;
                let frame = demo_frame(&mut renderer, &args, &cfg, t);
                lcd.send(&frame, &cfg)?;
            }
            println!(
                "{} frames in {:.2}s = {:.2} fps",
                count,
                start.elapsed().as_secs_f64(),
                count as f64 / start.elapsed().as_secs_f64()
            );
        }
        return Ok(());
    }
    app::run(args, cfg)
}

fn home_for(args: &Args) -> PathBuf {
    args.agent_home.clone().unwrap_or_else(|| {
        directories::UserDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .unwrap_or_default()
    })
}

fn demo_frame(
    renderer: &mut render::Renderer,
    args: &Args,
    settings: &config::Settings,
    t: f64,
) -> image::RgbaImage {
    let snapshot = metrics::Snapshot::demo_count(t, args.cores, args.demo_sessions as usize);
    let mut view = session::ViewState::default();
    view.sync(&snapshot.agents, &settings.agent_view, Instant::now(), true);
    let advance = if settings.agent_view.auto_rotate {
        (t / settings.agent_view.rotate_interval_seconds as f64) as usize
    } else {
        0
    };
    view.page = (args.demo_page.saturating_add(advance)) % view.pages();
    renderer.render_view(&snapshot, &view, t)
}
