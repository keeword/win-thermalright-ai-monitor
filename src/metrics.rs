use crate::agents::{AgentKind, Collector, Plan, Usage};
use std::{collections::HashMap, path::PathBuf};
use sysinfo::{
    Components, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub cpu: f32,
    pub cores: Vec<f32>,
    pub cpu_name: String,
    pub temperature: Option<f32>,
    pub total_memory: u64,
    pub used_memory: u64,
    pub available_memory: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub uptime: u64,
    pub processes: usize,
    pub agents: HashMap<AgentKind, Usage>,
}
impl Snapshot {
    pub fn animated(&self) -> bool {
        self.cpu > 55.0 || self.agents.values().any(|u| u.working || u.attention)
    }
    pub fn demo(t: f64, cores: u32) -> Self {
        let cores: Vec<_> = (0..cores)
            .map(|i| (25.0 + 55.0 * (0.5 + 0.5 * (t * 1.3 + i as f64 * 0.9).sin())) as f32)
            .collect();
        let cpu = cores.iter().sum::<f32>() / cores.len() as f32;
        let mut agents = HashMap::new();
        for kind in AgentKind::ALL {
            agents.insert(kind,Usage {available:true,project:"win-thermalright-ai-monitor".into(),
                message:match kind {AgentKind::Cursor=>"正在移植 Windows 实时仪表盘。\n\n| 模块 | 状态 |\n|---|---|\n| USB 协议 | 已完成 |\n| 系统监控 | 已完成 |\n| AI 面板 | 正在验证 |\n\n下一步：验证预览和硬件连接。",AgentKind::Codex=>"已完成 Rust USB 驱动与日志采集。\n\n• 增量读取本地会话记录\n• 今日 Token 用量与计划进度\n• 自动重连与自适应帧率\n\n正在执行构建和集成测试。",AgentKind::Claude=>"正在检查 Windows 原生实现。\n本地日志解析与图像渲染已就绪。"}.into(),
                model:match kind {AgentKind::Claude=>"claude-sonnet-4",AgentKind::Codex=>"gpt-5-codex",AgentKind::Cursor=>"auto"}.into(),
                input:12_480_000,output:186_400,working:true,age:2,quota_used:if kind==AgentKind::Codex {Some(23.0)} else {None},
                quota_reset:Some(chrono::Local::now().timestamp()+7200),context_used:Some(42.0),
                plan:Some(Plan {current:4,total:6,completed:3,text:"验证 Windows 预览与 USB 输出".into()}),..Usage::default()});
        }
        Self {
            cpu,
            cpu_name: format!("Windows CPU · {} logical cores", cores.len()),
            cores,
            temperature: Some(52.0),
            total_memory: 32 << 30,
            used_memory: 14 << 30,
            available_memory: 18 << 30,
            swap_used: 512 << 20,
            swap_total: 8 << 30,
            uptime: 27 * 3600 + 180,
            processes: 248,
            agents,
        }
    }
}

pub struct Metrics {
    system: System,
    components: Components,
    agents: Collector,
    cached_agents: HashMap<AgentKind, Usage>,
    temperature: Option<f32>,
    tick: u64,
}
impl Metrics {
    pub fn new(home: PathBuf) -> Self {
        Self {
            system: System::new_with_specifics(
                RefreshKind::nothing()
                    .with_cpu(sysinfo::CpuRefreshKind::everything())
                    .with_memory(MemoryRefreshKind::everything()),
            ),
            components: Components::new_with_refreshed_list(),
            agents: Collector::new(home),
            cached_agents: HashMap::new(),
            temperature: None,
            tick: 0,
        }
    }
    pub fn collect(&mut self) -> Snapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        if self.tick.is_multiple_of(4) {
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            self.components.refresh(true);
            self.temperature = self
                .components
                .iter()
                .filter(|c| {
                    let label = c.label().to_lowercase();
                    label.contains("cpu") || label.contains("package")
                })
                .filter_map(|c| c.temperature())
                .filter(|t| *t > 0.0 && *t < 150.0)
                .max_by(f32::total_cmp)
                .or_else(wmi_temperature);
            self.cached_agents = self.agents.collect();
        }
        self.tick += 1;
        Snapshot {
            cpu: self.system.global_cpu_usage(),
            cores: self.system.cpus().iter().map(|c| c.cpu_usage()).collect(),
            cpu_name: self
                .system
                .cpus()
                .first()
                .map(|c| c.brand().to_owned())
                .unwrap_or_default(),
            temperature: self.temperature,
            total_memory: self.system.total_memory(),
            used_memory: self.system.used_memory(),
            available_memory: self.system.available_memory(),
            swap_used: self.system.used_swap(),
            swap_total: self.system.total_swap(),
            uptime: System::uptime(),
            processes: self.system.processes().len(),
            agents: self.cached_agents.clone(),
        }
    }
}

#[cfg(windows)]
fn wmi_temperature() -> Option<f32> {
    thread_local! {
        static COM: Option<wmi::COMLibrary> = wmi::COMLibrary::new().ok();
    }
    let com = COM.with(|com| *com)?;
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Sensor {
        name: String,
        value: f32,
    }
    for namespace in ["ROOT\\LibreHardwareMonitor", "ROOT\\OpenHardwareMonitor"] {
        if let Ok(db) = wmi::WMIConnection::with_namespace_path(namespace, com)
            && let Ok(sensors) = db.raw_query::<Sensor>(
                "SELECT Name, Value FROM Sensor WHERE SensorType = 'Temperature'",
            )
        {
            let value = sensors
                .iter()
                .filter(|s| {
                    let name = s.name.to_lowercase();
                    name.contains("cpu") || name.contains("package") || name.contains("core")
                })
                .map(|s| s.value)
                .filter(|v| *v > 0.0 && *v < 150.0)
                .max_by(f32::total_cmp);
            if value.is_some() {
                return value;
            }
        }
    }
    None
}
#[cfg(not(windows))]
fn wmi_temperature() -> Option<f32> {
    None
}
