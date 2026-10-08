#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agents;
mod app;
mod config;
mod metrics;
#[cfg(windows)]
mod power;
mod protocol;
mod render;
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
}

fn main() {
    #[cfg(all(windows, not(debug_assertions)))]
    let headless = std::env::args().any(|arg| {
        matches!(
            arg.as_str(),
            "--snapshot" | "--gif" | "--benchmark" | "--diagnostics"
        )
    });
    #[cfg(all(windows, not(debug_assertions)))]
    let console = unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        ) != 0
    };
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

fn execute() -> Result<()> {
    let args = Args::parse();
    let cfg = config::Settings::load(args.config.as_deref())?;
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
        let agent_status: serde_json::Map<_,_> = s.agents.iter().map(|(kind,u)| (kind.name().to_owned(),serde_json::json!({"available":u.available,"working":u.working,"waiting":u.waiting,"today_input":u.input,"today_output":u.output,"project":u.project,"session_id":u.session_id,"log_path":u.log_path,"last_activity":u.last_activity,"message_characters":u.message.chars().count()}))).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"cpu":s.cpu_name,"cpu_percent":s.cpu,"logical_cores":s.cores.len(),"memory_total":s.total_memory,"memory_used":s.used_memory,"cpu_temperature":s.temperature,"processes":s.processes,"uptime_seconds":s.uptime,"agents":agent_status})
            )?
        );
        return Ok(());
    }
    if args.snapshot.is_some() || args.gif.is_some() || args.benchmark.is_some() {
        let mut renderer = render::Renderer::new(cfg.font.as_deref())?;
        if let Some(path) = &args.snapshot {
            renderer
                .render(&metrics::Snapshot::demo(0.0, args.cores), &cfg, 0.0)
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
                let frame = renderer.render(&metrics::Snapshot::demo(t, args.cores), &cfg, t);
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
                let frame = renderer.render(&metrics::Snapshot::demo(t, args.cores), &cfg, t);
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
