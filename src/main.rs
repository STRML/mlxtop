// SPDX-License-Identifier: MIT

#![forbid(unsafe_code)]

//! A dark terminal monitor for macOS memory pressure and local LLM serving.
//!
//! The collector owns the sampling path used by both the interactive TUI and
//! the static report. Provider-specific telemetry is optional; missing data
//! remains explicitly unavailable instead of being inferred.

mod chart_navigation;
mod chart_scale;
mod diagnosis;
mod gpu;
mod gpu_dashboard;
mod host;
mod model_dashboard;
mod operator_charts;
mod process_memory;
mod providers;
mod request_dashboard;
mod swap_usage;

use chart_navigation::Chart;
use host::{Host, Platform};
use std::any::Any;
use std::backtrace::Backtrace;
use std::collections::VecDeque;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, stdout, IsTerminal, Read, Seek, SeekFrom, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{json, Value};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState, Tabs, Wrap,
    },
    Frame, Terminal,
};

const GREEN: Color = Color::Rgb(88, 211, 147);
const YELLOW: Color = Color::Rgb(246, 193, 79);
const RED: Color = Color::Rgb(248, 113, 113);
const CYAN: Color = Color::Rgb(90, 202, 225);
const BLUE: Color = Color::Rgb(120, 153, 255);
const MUTED: Color = Color::Rgb(139, 151, 168);
const DIM: Color = Color::Rgb(78, 90, 108);
const PANEL: Color = Color::Rgb(18, 24, 34);
const PANEL_RAISED: Color = Color::Rgb(24, 32, 46);
const EDGE: Color = Color::Rgb(54, 68, 88);
const VERSION: &str = env!("CARGO_PKG_VERSION");
const MIB: u64 = 1024 * 1024;
const MEMORY_WARN_LOAD: u64 = 70;
const MEMORY_CRITICAL_LOAD: u64 = 85;
const GPU_WARN_LOAD: u64 = 75;
const GPU_CRITICAL_LOAD: u64 = 90;
const SWAP_WARN_RATE: u64 = MIB;
const SWAP_CRITICAL_RATE: u64 = 16 * MIB;
const COMPRESSION_WARN_RATE: u64 = 64 * MIB;
const SWAP_WARN_EXIT: u64 = 2 * MIB;
/** Churn rate that turns "paging active" on; `swap_warn_exit` turns it off. */
const PAGING_ACTIVE_ENTER_RATE: u64 = 4 * MIB;
/** GPU load that turns "GPU busy" on; `gpu_warn_exit` turns it off. */
const GPU_BUSY_ENTER_LOAD: u64 = 80;
const COMPRESSION_WARN_EXIT: u64 = 32 * MIB;
const GPU_WARN_EXIT: u64 = 70;
const CORRELATION_HISTORY_LIMIT: usize = 16;
const THROUGHPUT_CHANGE_MIN_TPS: f64 = 2.0;
const THROUGHPUT_CHANGE_RATIO: f64 = 0.10;
const CHART_VISUAL_DEADBAND_FRACTION: f64 = 0.25;
const CONTEXT_GROWTH_TOKENS: u64 = 1024;
const MODEL_MEMORY_GROWTH: u64 = 256 * MIB;
const MAX_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const DIAGNOSTICS_LOG_ENV: &str = "MLXTOP_LOG_PATH";
const DIAGNOSTICS_MAX_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_OMLX_HOST: &str = "127.0.0.1";
const DEFAULT_OMLX_PORT: u16 = 8080;

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
struct Config {
    interval: Option<u64>,
    history: Option<usize>,
    omx: Option<OmxConfig>,
    memory_warn_load: Option<u64>,
    memory_critical_load: Option<u64>,
    gpu_warn_load: Option<u64>,
    gpu_critical_load: Option<u64>,
    swap_warn_rate: Option<u64>,
    swap_critical_rate: Option<u64>,
    compression_warn_rate: Option<u64>,
    swap_warn_exit: Option<u64>,
    compression_warn_exit: Option<u64>,
    gpu_warn_exit: Option<u64>,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
struct OmxConfig {
    host: Option<String>,
    port: Option<u16>,
}

fn load_config() -> Config {
    load_config_from(&config_path())
}

fn load_config_from(config_path: &Path) -> Config {
    match fs::read_to_string(config_path) {
        Ok(text) => match serde_json::from_str::<Config>(&text) {
            Ok(config) => {
                diagnostics_log(
                    "INFO",
                    "config_loaded",
                    format!("path={}", config_path.display()),
                );
                config
            }
            Err(e) => {
                diagnostics_log(
                    "WARN",
                    "config_parse_error",
                    format!("path={} error={}", config_path.display(), e),
                );
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

const INTERVAL_MIN: u64 = 1;
const INTERVAL_MAX: u64 = 60;
const INTERVAL_DEFAULT: u64 = 1;
const HISTORY_MIN: usize = 20;
const HISTORY_MAX: usize = 3600;
const HISTORY_DEFAULT: usize = 300;

/**
 * Resolve `interval` from the config file.
 *
 * An out-of-range entry falls back to the default and reports `true` so the
 * caller can log it: a typo in a file the user edits by hand should not stop
 * mlxtop from starting. An out-of-range CLI argument is still a hard error,
 * because the user typed it just now and can see the message.
 */
fn config_interval(config: &Config) -> (u64, bool) {
    match config.interval {
        Some(value) if (INTERVAL_MIN..=INTERVAL_MAX).contains(&value) => (value, false),
        Some(_) => (INTERVAL_DEFAULT, true),
        None => (INTERVAL_DEFAULT, false),
    }
}

/**
 * Resolve `history` from the config file, with the same fallback rule as
 * [`config_interval`].
 */
fn config_history(config: &Config) -> (usize, bool) {
    match config.history {
        Some(value) if (HISTORY_MIN..=HISTORY_MAX).contains(&value) => (value, false),
        Some(_) => (HISTORY_DEFAULT, true),
        None => (HISTORY_DEFAULT, false),
    }
}

fn config_path() -> PathBuf {
    config_path_in(env::var_os("HOME").map(PathBuf::from))
}

fn config_path_in(home: Option<PathBuf>) -> PathBuf {
    if let Some(home) = home {
        home.join(".config/mlxtop/config.json")
    } else {
        PathBuf::from("config.json")
    }
}

/**
 * Effective severity thresholds for one run.
 *
 * Defaults mirror the built-in constants; every field can be replaced by the
 * matching key in `~/.config/mlxtop/config.json`. Values are resolved once at
 * startup and then handed to the severity, alert and correlation code, so a
 * configured value changes what the user actually sees.
 */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Thresholds {
    memory_warn_load: u64,
    memory_critical_load: u64,
    gpu_warn_load: u64,
    gpu_critical_load: u64,
    gpu_warn_exit: u64,
    swap_warn_rate: u64,
    swap_critical_rate: u64,
    swap_warn_exit: u64,
    compression_warn_rate: u64,
    compression_warn_exit: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            memory_warn_load: MEMORY_WARN_LOAD,
            memory_critical_load: MEMORY_CRITICAL_LOAD,
            gpu_warn_load: GPU_WARN_LOAD,
            gpu_critical_load: GPU_CRITICAL_LOAD,
            gpu_warn_exit: GPU_WARN_EXIT,
            swap_warn_rate: SWAP_WARN_RATE,
            swap_critical_rate: SWAP_CRITICAL_RATE,
            swap_warn_exit: SWAP_WARN_EXIT,
            compression_warn_rate: COMPRESSION_WARN_RATE,
            compression_warn_exit: COMPRESSION_WARN_EXIT,
        }
    }
}

impl Thresholds {
    fn from_config(config: &Config) -> Self {
        let defaults = Self::default();
        Self {
            memory_warn_load: config.memory_warn_load.unwrap_or(defaults.memory_warn_load),
            memory_critical_load: config
                .memory_critical_load
                .unwrap_or(defaults.memory_critical_load),
            gpu_warn_load: config.gpu_warn_load.unwrap_or(defaults.gpu_warn_load),
            gpu_critical_load: config
                .gpu_critical_load
                .unwrap_or(defaults.gpu_critical_load),
            gpu_warn_exit: config.gpu_warn_exit.unwrap_or(defaults.gpu_warn_exit),
            swap_warn_rate: config.swap_warn_rate.unwrap_or(defaults.swap_warn_rate),
            swap_critical_rate: config
                .swap_critical_rate
                .unwrap_or(defaults.swap_critical_rate),
            swap_warn_exit: config.swap_warn_exit.unwrap_or(defaults.swap_warn_exit),
            compression_warn_rate: config
                .compression_warn_rate
                .unwrap_or(defaults.compression_warn_rate),
            compression_warn_exit: config
                .compression_warn_exit
                .unwrap_or(defaults.compression_warn_exit),
        }
        .normalized()
    }

    /**
     * Keep the bands usable no matter what the file says: percentages stay
     * within 0..=100, a critical level never sits below its warning level,
     * and a hysteresis exit never sits above the level that turns the state
     * on. Bad input is clamped rather than rejected so a single stray value
     * cannot silently remove a severity band.
     */
    fn normalized(mut self) -> Self {
        self.memory_warn_load = self.memory_warn_load.min(100);
        self.memory_critical_load = self.memory_critical_load.clamp(self.memory_warn_load, 100);
        self.gpu_warn_load = self.gpu_warn_load.min(100);
        self.gpu_critical_load = self.gpu_critical_load.clamp(self.gpu_warn_load, 100);
        self.gpu_warn_exit = self.gpu_warn_exit.min(100);
        self.swap_critical_rate = self.swap_critical_rate.max(self.swap_warn_rate);
        self.compression_warn_exit = self.compression_warn_exit.min(self.compression_warn_rate);
        self
    }
}

/** Shared warn/critical banding used by every load-style indicator. */
fn load_tone(value: u64, warn: u64, critical: u64) -> Tone {
    if value >= critical {
        Tone::Red
    } else if value >= warn {
        Tone::Yellow
    } else {
        Tone::Green
    }
}

static DIAGNOSTICS: OnceLock<Diagnostics> = OnceLock::new();

struct Diagnostics {
    path: PathBuf,
    file: Mutex<File>,
    max_bytes: u64,
}

impl Diagnostics {
    fn open(path: PathBuf, max_bytes: u64) -> Option<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).ok()?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(Self {
            path,
            file: Mutex::new(file),
            max_bytes,
        })
    }

    fn log(&self, level: &str, event: &str, details: &str) {
        let details = details.replace(['\r', '\n'], "\\n");
        if let Ok(mut file) = self.file.lock() {
            if file
                .metadata()
                .map(|metadata| metadata.len() >= self.max_bytes)
                .unwrap_or(false)
            {
                let rotated = self.path.with_extension("log.1");
                let _ = file.flush();
                if fs::rename(&self.path, rotated).is_ok() {
                    if let Ok(replacement) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.path)
                    {
                        *file = replacement;
                    }
                } else if file.set_len(0).is_ok() {
                    let _ = file.seek(SeekFrom::Start(0));
                }
            }
            let _ = writeln!(
                file,
                "ts_ms={} level={} event={} {}",
                diagnostics_timestamp_ms(),
                level,
                event,
                details
            );
            let _ = file.flush();
        }
    }
}

fn init_diagnostics() -> Option<&'static Diagnostics> {
    install_diagnostics(diagnostics_path()?)
}

/// Open the process-wide log at `path` unless one is already open, then
/// record the session start in whichever log is active.
fn install_diagnostics(path: PathBuf) -> Option<&'static Diagnostics> {
    if DIAGNOSTICS.get().is_none() {
        let _ = DIAGNOSTICS.set(Diagnostics::open(path, DIAGNOSTICS_MAX_BYTES)?);
    }
    let diagnostics = DIAGNOSTICS.get()?;
    diagnostics.log(
        "INFO",
        "session_start",
        &format!(
            "version={} pid={} log_path={}",
            VERSION,
            std::process::id(),
            log_field(&diagnostics.path.display().to_string())
        ),
    );
    Some(diagnostics)
}

fn diagnostics_path() -> Option<PathBuf> {
    diagnostics_path_from(
        env::var(DIAGNOSTICS_LOG_ENV).ok(),
        env::var("XDG_STATE_HOME").ok(),
        env::var_os("HOME").map(PathBuf::from),
        Platform::current(),
    )
}

fn diagnostics_path_from(
    log_path: Option<String>,
    state_home: Option<String>,
    home: Option<PathBuf>,
    platform: Platform,
) -> Option<PathBuf> {
    if let Some(path) = log_path {
        let path = path.trim();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    if platform == Platform::Linux {
        if let Some(state_home) = state_home {
            let state_home = state_home.trim();
            if !state_home.is_empty() {
                return Some(PathBuf::from(state_home).join("mlxtop/mlxtop.log"));
            }
        }
        return home
            .map(|home| home.join(".local/state/mlxtop/mlxtop.log"))
            .or_else(|| Some(PathBuf::from("mlxtop.log")));
    }
    home.map(|home| home.join("Library/Logs/mlxtop/mlxtop.log"))
        .or_else(|| Some(PathBuf::from("mlxtop.log")))
}

fn diagnostics_default_hint(platform: Platform) -> &'static str {
    if platform == Platform::Linux {
        "~/.local/state/mlxtop/mlxtop.log"
    } else {
        "~/Library/Logs/mlxtop/mlxtop.log"
    }
}

fn diagnostics_log(level: &str, event: &str, details: impl AsRef<str>) {
    if let Some(diagnostics) = DIAGNOSTICS.get() {
        diagnostics.log(level, event, details.as_ref());
    }
}

fn diagnostics_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn log_field(value: &str) -> String {
    let mut field = String::new();
    for character in value.chars().take(160) {
        if character.is_ascii_alphanumeric()
            || matches!(character, '-' | '_' | '.' | '/' | ':' | '%' | '@')
        {
            field.push(character);
        } else if character.is_whitespace() {
            field.push('_');
        } else {
            field.push('?');
        }
    }
    field
}

fn log_optional_f64(value: Option<f64>) -> String {
    value
        .filter(|value| value.is_finite())
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "na".into())
}

fn log_optional_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "na".into())
}

fn log_optional_u8(value: Option<u8>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "na".into())
}

fn panic_payload(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".into()
    }
}

fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown".into());
        let message = panic_payload(info.payload());
        let backtrace = Backtrace::force_capture();
        diagnostics_log(
            "ERROR",
            "panic",
            format!(
                "message={} location={} backtrace={backtrace}",
                log_field(&message),
                log_field(&location),
            ),
        );
        previous(info);
    }));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Green,
    Yellow,
    Red,
    Cyan,
    Blue,
    Muted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JournalFilter {
    All,
    Llm,
    Pressure,
    Paging,
    Gpu,
    Thermal,
    System,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TelemetrySource {
    #[default]
    None,
    Live,
    Log,
    Report,
}

impl TelemetrySource {
    fn label(self) -> &'static str {
        match self {
            Self::None => "unavailable",
            Self::Live => "live API",
            Self::Log => "completion log",
            Self::Report => "reported usage",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventKind {
    System,
    Llm,
    Queue,
    Pressure,
    Paging,
    Gpu,
    Thermal,
}

impl EventKind {
    fn from_state(state: &str) -> Self {
        match state {
            "LLM" | "PROMPT" => Self::Llm,
            "QUEUE" => Self::Queue,
            "PRESSURE" | "MEMORY BOTTLENECK" | "MEMORY STRESS" => Self::Pressure,
            "PAGING" | "SWAP THRASHING" | "HEAVY PAGING" | "PAGE-IN RECOVERY" | "PAGING ACTIVE"
            | "WATCH PAGING" => Self::Paging,
            "GPU" => Self::Gpu,
            "THERMAL" => Self::Thermal,
            _ => Self::System,
        }
    }
}

impl JournalFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Llm => "LLM",
            Self::Pressure => "PRESSURE",
            Self::Paging => "PAGING",
            Self::Gpu => "GPU",
            Self::Thermal => "THERMAL",
            Self::System => "SYSTEM",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::Llm,
            Self::Llm => Self::Pressure,
            Self::Pressure => Self::Paging,
            Self::Paging => Self::Gpu,
            Self::Gpu => Self::Thermal,
            Self::Thermal => Self::System,
            Self::System => Self::All,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::All => Self::System,
            Self::Llm => Self::All,
            Self::Pressure => Self::Llm,
            Self::Paging => Self::Pressure,
            Self::Gpu => Self::Paging,
            Self::Thermal => Self::Gpu,
            Self::System => Self::Thermal,
        }
    }

    fn matches(self, kind: EventKind) -> bool {
        match self {
            Self::All => true,
            Self::Llm => matches!(kind, EventKind::Llm | EventKind::Queue),
            Self::Pressure => kind == EventKind::Pressure,
            Self::Paging => kind == EventKind::Paging,
            Self::Gpu => kind == EventKind::Gpu,
            Self::Thermal => kind == EventKind::Thermal,
            Self::System => kind == EventKind::System,
        }
    }
}

impl Tone {
    fn color(self) -> Color {
        match self {
            Self::Green => GREEN,
            Self::Yellow => YELLOW,
            Self::Red => RED,
            Self::Cyan => CYAN,
            Self::Blue => BLUE,
            Self::Muted => MUTED,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChartMetric {
    Generation,
    Prefill,
    Cache,
    Memory,
    Swap,
    Gpu,
}

impl ChartMetric {
    fn chart_tone(self) -> Tone {
        match self {
            Self::Generation | Self::Prefill | Self::Cache => Tone::Cyan,
            Self::Memory => Tone::Cyan,
            Self::Swap => Tone::Yellow,
            Self::Gpu => Tone::Blue,
        }
    }

    fn tone(self, value: u64, thresholds: Thresholds) -> Tone {
        match self {
            Self::Generation | Self::Prefill | Self::Cache => Tone::Cyan,
            // Linux derives pressure from unavailable memory (MemAvailable),
            // using these bands. Resident RAM history records pressure from
            // the sample instead; occupied file cache is not a pressure signal.
            Self::Memory => load_tone(
                value,
                thresholds.memory_warn_load,
                thresholds.memory_critical_load,
            ),
            Self::Gpu => load_tone(
                value,
                thresholds.gpu_warn_load,
                thresholds.gpu_critical_load,
            ),
            Self::Swap => load_tone(
                value,
                thresholds.swap_warn_rate,
                thresholds.swap_critical_rate,
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct ChartPoint {
    value: Option<u64>,
    tone: Tone,
    observed_at: SystemTime,
}

#[derive(Clone, Copy)]
struct TraceCell {
    glyph: char,
    tone: Tone,
}

#[derive(Clone, Copy)]
struct RenderPoint {
    value: Option<u64>,
    tone: Tone,
    break_before: bool,
}

#[derive(Default)]
struct Consumer {
    name: String,
    rss: u64,
    processes: u32,
}

#[derive(Clone)]
struct LlmProcess {
    pid: u32,
    name: String,
    command: String,
    rss: u64,
    cpu: f64,
    memory_percent: Option<f64>,
    state: String,
    pageins: Option<u64>,
    pagein_rate: Option<f64>,
}

struct ProcessSnapshot {
    llm_count: u32,
    llm_rss: u64,
    llm_cpu: f64,
    top_llm: Option<LlmProcess>,
    provider: Option<String>,
    largest_consumer: Option<String>,
    llm_processes: Vec<LlmProcess>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopSort {
    Rss,
    Cpu,
    Pid,
    Name,
}

impl TopSort {
    fn label(self) -> &'static str {
        match self {
            Self::Rss => "RSS",
            Self::Cpu => "CPU",
            Self::Pid => "PID",
            Self::Name => "NAME",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Rss => Self::Cpu,
            Self::Cpu => Self::Pid,
            Self::Pid => Self::Name,
            Self::Name => Self::Rss,
        }
    }
}

#[derive(Clone)]
struct SignalEvent {
    time: String,
    recorded_at: SystemTime,
    kind: EventKind,
    state: String,
    summary: String,
    tone: Tone,
}

#[derive(Default)]
struct LlmLogStats {
    model: Option<String>,
    tokens_per_second: Option<f64>,
    output_tokens: Option<u64>,
    prompt_tokens: Option<u64>,
    observed_at: Option<SystemTime>,
}

/// MLX allocator and device counters reported by the serving runtime.
///
/// MLX keeps allocator state inside the process that owns the arrays.  The
/// monitor therefore never invents these values from RSS or GPU utilization;
/// they are populated only from a provider endpoint that can observe the
/// serving process directly.
#[derive(Clone, Default)]
struct MlxTelemetry {
    version: Option<String>,
    active_memory: Option<u64>,
    cache_memory: Option<u64>,
    peak_memory: Option<u64>,
    device_name: Option<String>,
    architecture: Option<String>,
    memory_size: Option<u64>,
    recommended_working_set: Option<u64>,
    max_buffer_size: Option<u64>,
    resource_limit: Option<u64>,
    process_footprint: Option<u64>,
}

#[derive(Clone, Default)]
struct MetalTelemetry {
    device_name: Option<String>,
    architecture: Option<String>,
    gpu_cores: Option<u16>,
    renderer_util: Option<u8>,
    tiler_util: Option<u8>,
    resource_limit: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ThroughputDirection {
    #[default]
    Unknown,
    Down,
    Up,
    Flat,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CorrelationCause {
    #[default]
    None,
    Paging,
    Compression,
    MemoryPressure,
    Thermal,
    MetalMemory,
    GpuSaturation,
    Queueing,
    ContextGrowth,
    ModelMemory,
    Runtime,
}

impl CorrelationCause {
    fn label(self) -> &'static str {
        match self {
            Self::None => "no correlated cause",
            Self::Paging => "paging",
            Self::Compression => "compression churn",
            Self::MemoryPressure => "memory pressure",
            Self::Thermal => "thermal limiting",
            Self::MetalMemory => "Metal memory pressure",
            Self::GpuSaturation => "GPU saturation",
            Self::Queueing => "queueing",
            Self::ContextGrowth => "context/KV growth",
            Self::ModelMemory => "model memory growth",
            Self::Runtime => "workload/runtime change",
        }
    }

    fn tone(self) -> Tone {
        match self {
            Self::Paging | Self::MemoryPressure | Self::Thermal => Tone::Red,
            Self::Compression
            | Self::MetalMemory
            | Self::GpuSaturation
            | Self::Queueing
            | Self::ContextGrowth
            | Self::ModelMemory
            | Self::Runtime => Tone::Yellow,
            Self::None => Tone::Muted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CorrelationKey {
    direction: ThroughputDirection,
    cause: CorrelationCause,
}

#[derive(Clone, Default)]
struct CorrelationInsight {
    direction: ThroughputDirection,
    cause: CorrelationCause,
    confidence: u8,
    summary: String,
    details: String,
    event_key: Option<CorrelationKey>,
}

impl CorrelationInsight {
    fn is_material_drop(&self) -> bool {
        self.direction == ThroughputDirection::Down
    }

    fn tone(&self) -> Tone {
        match self.direction {
            ThroughputDirection::Down => {
                if matches!(
                    self.cause,
                    CorrelationCause::Paging
                        | CorrelationCause::MemoryPressure
                        | CorrelationCause::Thermal
                ) {
                    Tone::Red
                } else {
                    Tone::Yellow
                }
            }
            ThroughputDirection::Up => Tone::Green,
            ThroughputDirection::Unknown | ThroughputDirection::Flat => self.cause.tone(),
        }
    }

    fn confidence_label(&self) -> &'static str {
        match self.confidence {
            80..=u8::MAX => "high",
            55..=79 => "medium",
            1..=54 => "low",
            _ => "unavailable",
        }
    }
}

#[derive(Clone, Default)]
struct CorrelationObservation {
    provider: String,
    model: String,
    generation_tps: Option<f64>,
    gpu_util: Option<u8>,
    renderer_util: Option<u8>,
    tiler_util: Option<u8>,
    paging_rate: u64,
    compression_rate: u64,
    pressure: u8,
    thermal_limited: bool,
    model_memory: Option<u64>,
    model_memory_max: Option<u64>,
    metal_in_use: Option<u64>,
    metal_alloc: Option<u64>,
    context_tokens: Option<u64>,
    active_requests: Option<u64>,
    waiting_requests: Option<u64>,
}

#[derive(Default)]
struct CorrelationEngine {
    observations: VecDeque<CorrelationObservation>,
}

#[derive(Clone, Default)]
struct LlmTelemetry {
    source: TelemetrySource,
    observed_at: Option<SystemTime>,
    provider: Option<String>,
    status: Option<String>,
    model: Option<String>,
    generation_tps: Option<f64>,
    generation_tps_live: bool,
    prefill_tps: Option<f64>,
    prefill_tps_live: bool,
    output_tokens: Option<u64>,
    prompt_tokens: Option<u64>,
    requests: Vec<providers::RequestUsage>,
    cache_efficiency: Option<f64>,
    prefix_hit_rate: Option<f64>,
    total_prompt_tokens: Option<u64>,
    total_cached_tokens: Option<u64>,
    active_requests: Option<u64>,
    waiting_requests: Option<u64>,
    model_memory: Option<u64>,
    model_memory_max: Option<u64>,
    details: Option<String>,
    remote: bool,
    cache_interval_efficiency: Option<f64>,
    mlx: MlxTelemetry,
}

struct LlmTelemetryClient {
    provider_adapter: providers::Adapter,
    home: Option<PathBuf>,
    host: String,
    port: u16,
    session_cookie: Option<String>,
    cached: Option<LlmTelemetry>,
    mlx_metadata: MlxTelemetry,
    last_stats_available: Option<bool>,
    next_metadata_poll: Instant,
    next_poll: Instant,
    retry_backoff: Duration,
}

#[derive(Clone)]
struct Sample {
    updated: String,
    pressure: String,
    pressure_meaning: String,
    pressure_tone: Tone,
    availability: Option<u8>,
    // Physical RAM occupied, including file cache. Separate from macOS's
    // memorystatus level, which includes pageable application memory.
    resident_memory: Option<u64>,
    total_memory: u64,
    wired: u64,
    compressor: u64,
    compressed_logical: u64,
    anonymous: u64,
    file_backed: u64,
    swap_total: u64,
    swap_used: u64,
    swap_available: bool,
    swap_in: u64,
    swap_out: u64,
    swap_growth: i64,
    compress: u64,
    decompress: u64,
    reactivated: u64,
    vm_available: bool,
    gpu_util: Option<u8>,
    gpu_alloc: Option<u64>,
    gpu_in_use: Option<u64>,
    gpus: Vec<gpu::Device>,
    metal: MetalTelemetry,
    mlx: MlxTelemetry,
    thermal: String,
    llm_count: u32,
    llm_rss: u64,
    process_memory: Option<process_memory::Reading>,
    process_memory_growth: Option<i64>,
    llm_cpu: f64,
    llm_processes: Vec<LlmProcess>,
    largest_consumer: Option<String>,
    llm_model: String,
    llm_provider: String,
    llm_status: String,
    llm_source: TelemetrySource,
    llm_observed_at: Option<SystemTime>,
    llm_generation_tps: Option<f64>,
    llm_generation_tps_live: bool,
    llm_prefill_tps: Option<f64>,
    llm_prefill_tps_live: bool,
    llm_output_tokens: Option<u64>,
    llm_prompt_tokens: Option<u64>,
    llm_requests: Vec<providers::RequestUsage>,
    llm_cache_efficiency: Option<f64>,
    llm_cache_interval_efficiency: Option<f64>,
    llm_prefix_hit_rate: Option<f64>,
    llm_active_requests: Option<u64>,
    llm_waiting_requests: Option<u64>,
    llm_model_memory: Option<u64>,
    llm_model_memory_max: Option<u64>,
    llm_details: Option<String>,
    llm_remote: bool,
    correlation: CorrelationInsight,
    llm_top: String,
    llm_pid: u32,
    impact: String,
    impact_tone: Tone,
    health: Option<u16>,
    grade: String,
    limiter: String,
    guidance_badge: String,
    guidance_cause: String,
    guidance_action: String,
    rate_ready: bool,
}

impl Sample {
    fn has_nvidia_gpus(&self) -> bool {
        cfg!(target_os = "linux") && !self.gpus.is_empty()
    }
}

impl Default for Sample {
    fn default() -> Self {
        Self {
            updated: "waiting".into(),
            pressure: "UNKNOWN".into(),
            pressure_meaning: "unavailable".into(),
            pressure_tone: Tone::Muted,
            availability: None,
            resident_memory: None,
            total_memory: 0,
            wired: 0,
            compressor: 0,
            compressed_logical: 0,
            anonymous: 0,
            file_backed: 0,
            swap_total: 0,
            swap_used: 0,
            swap_available: false,
            swap_in: 0,
            swap_out: 0,
            swap_growth: 0,
            compress: 0,
            decompress: 0,
            reactivated: 0,
            vm_available: false,
            gpu_util: None,
            gpu_alloc: None,
            gpu_in_use: None,
            gpus: Vec::new(),
            metal: MetalTelemetry::default(),
            mlx: MlxTelemetry::default(),
            thermal: "unavailable".into(),
            llm_count: 0,
            llm_rss: 0,
            process_memory: None,
            process_memory_growth: None,
            llm_cpu: 0.0,
            llm_processes: Vec::new(),
            largest_consumer: None,
            llm_model: "not detected".into(),
            llm_provider: "not detected".into(),
            llm_status: "offline".into(),
            llm_source: TelemetrySource::None,
            llm_observed_at: None,
            llm_generation_tps: None,
            llm_generation_tps_live: false,
            llm_prefill_tps: None,
            llm_prefill_tps_live: false,
            llm_output_tokens: None,
            llm_prompt_tokens: None,
            llm_requests: Vec::new(),
            llm_cache_efficiency: None,
            llm_cache_interval_efficiency: None,
            llm_prefix_hit_rate: None,
            llm_active_requests: None,
            llm_waiting_requests: None,
            llm_model_memory: None,
            llm_model_memory_max: None,
            llm_details: None,
            llm_remote: false,
            correlation: CorrelationInsight::default(),
            llm_top: "none".into(),
            llm_pid: 0,
            impact: "SAMPLING".into(),
            impact_tone: Tone::Cyan,
            health: None,
            grade: "SAMPLING".into(),
            limiter: "collecting baseline".into(),
            guidance_badge: "WAIT".into(),
            guidance_cause: "Collecting the live-rate baseline.".into(),
            guidance_action: "Wait one refresh before acting.".into(),
            rate_ready: false,
        }
    }
}

struct PreviousCounters {
    at: Instant,
    swapins: u64,
    swapouts: u64,
    compressions: u64,
    decompressions: u64,
    reactivations: u64,
    swap_used: u64,
}

#[derive(Clone)]
struct CollectorView {
    current: Sample,
    generation_history: VecDeque<ChartPoint>,
    prefill_history: VecDeque<ChartPoint>,
    cache_history: VecDeque<ChartPoint>,
    load_history: VecDeque<ChartPoint>,
    swap_history: VecDeque<ChartPoint>,
    gpu_history: VecDeque<ChartPoint>,
    signals: VecDeque<SignalEvent>,
    request_history: request_dashboard::History,
    operator_history: operator_charts::History,
}

#[derive(Default)]
struct VmCounters {
    free: Option<u64>,
    speculative: Option<u64>,
    wired: u64,
    compressor: u64,
    compressed_logical: u64,
    anonymous: u64,
    file_backed: u64,
    swapins: u64,
    swapouts: u64,
    compressions: u64,
    decompressions: u64,
    reactivations: u64,
}

#[derive(Clone)]
struct CacheCounters {
    provider: Option<String>,
    prompt_tokens: u64,
    cached_tokens: u64,
}

struct Collector {
    host: Box<dyn Host>,
    platform: Platform,
    home: Option<PathBuf>,
    page_size: u64,
    total_memory: u64,
    metal: MetalTelemetry,
    llm_client: LlmTelemetryClient,
    seen_requests: VecDeque<(String, u64)>,
    correlation: CorrelationEngine,
    previous: Option<PreviousCounters>,
    previous_llm_cache: Option<CacheCounters>,
    current: Sample,
    generation_history: VecDeque<ChartPoint>,
    prefill_history: VecDeque<ChartPoint>,
    cache_history: VecDeque<ChartPoint>,
    load_history: VecDeque<ChartPoint>,
    swap_history: VecDeque<ChartPoint>,
    gpu_history: VecDeque<ChartPoint>,
    signals: VecDeque<SignalEvent>,
    request_history: request_dashboard::History,
    operator_history: operator_charts::History,
    history_limit: usize,
    thresholds: Thresholds,
}

enum SamplerCommand {
    SetPaused(bool),
    SetInterval(Duration),
    Reset,
    Stop,
}

struct Sampler {
    commands: Sender<SamplerCommand>,
    views: Receiver<CollectorView>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Sampler {
    fn spawn(interval: Duration, history_limit: usize, config: Config) -> Self {
        diagnostics_log(
            "INFO",
            "sampler_start",
            format!(
                "interval_seconds={} history_limit={history_limit}",
                interval.as_secs()
            ),
        );
        Self::start(interval, move || Collector::new(history_limit, config))
    }

    /// Run `build`'s collector on the sampling thread. The collector is built
    /// there because its first host reads may block on slow commands.
    fn start(interval: Duration, build: impl FnOnce() -> Collector + Send + 'static) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (view_tx, view_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut collector = build();
            let mut interval = interval;
            let mut paused = false;
            let mut next_sample = Instant::now();

            loop {
                if !paused && Instant::now() >= next_sample {
                    let sampled = panic::catch_unwind(AssertUnwindSafe(|| collector.sample()));
                    if let Err(payload) = sampled.as_ref() {
                        diagnostics_log(
                            "ERROR",
                            "sampler_panic",
                            format!("message={}", log_field(&panic_payload(payload.as_ref()))),
                        );
                    }
                    if sampled.is_err() {
                        break;
                    }
                    if view_tx.send(collector.view()).is_err() {
                        diagnostics_log("INFO", "sampler_stop", "reason=view_receiver_closed");
                        break;
                    }
                    next_sample = Instant::now() + interval;
                }

                let wait = if paused {
                    Duration::from_millis(100)
                } else {
                    next_sample
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(100))
                };
                match command_rx.recv_timeout(wait) {
                    Ok(SamplerCommand::SetPaused(value)) => {
                        paused = value;
                        diagnostics_log("INFO", "sampler_paused", format!("paused={paused}"));
                        if !paused {
                            next_sample = Instant::now();
                        }
                    }
                    Ok(SamplerCommand::SetInterval(value)) => {
                        interval = value;
                        diagnostics_log(
                            "INFO",
                            "sampler_interval_changed",
                            format!("interval_seconds={}", interval.as_secs()),
                        );
                        next_sample = Instant::now() + interval;
                    }
                    Ok(SamplerCommand::Reset) => {
                        collector.reset();
                        diagnostics_log("INFO", "sampler_reset", "history_and_baselines_cleared");
                        next_sample = Instant::now();
                    }
                    Ok(SamplerCommand::Stop) => {
                        diagnostics_log("INFO", "sampler_stop", "reason=shutdown");
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        diagnostics_log("WARN", "sampler_stop", "reason=command_channel_closed");
                        break;
                    }
                }
            }
        });
        Self {
            commands: command_tx,
            views: view_rx,
            handle: Some(handle),
        }
    }

    fn send(&self, command: SamplerCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        let _ = self.commands.send(SamplerCommand::Stop);
        if let Some(handle) = self.handle.take() {
            if let Err(payload) = handle.join() {
                diagnostics_log(
                    "ERROR",
                    "sampler_thread_panic",
                    format!("message={}", log_field(&panic_payload(payload.as_ref()))),
                );
            }
        }
    }
}

impl Collector {
    fn new(history_limit: usize, config: Config) -> Self {
        Self::with_host(
            history_limit,
            config,
            Box::new(host::System),
            Platform::current(),
            env::var_os("HOME").map(PathBuf::from),
        )
    }

    fn with_host(
        history_limit: usize,
        config: Config,
        host: Box<dyn Host>,
        platform: Platform,
        home: Option<PathBuf>,
    ) -> Self {
        let (total_memory, page_size, metal) = match platform {
            Platform::MacOs => {
                let total_memory = host
                    .command_u64("/usr/sbin/sysctl", &["-n", "hw.memsize"])
                    .unwrap_or(0);
                let mut metal = parse_metal_hardware(
                    &host
                        .command(
                            "/usr/sbin/ioreg",
                            &["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"],
                        )
                        .unwrap_or_default(),
                );
                metal.architecture = host
                    .command("/usr/sbin/sysctl", &["-n", "hw.machine"])
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty());
                metal.resource_limit = host
                    .command_u64("/usr/sbin/sysctl", &["-n", "iogpu.wired_limit_mb"])
                    .filter(|value| *value > 0)
                    .map(|value| value.saturating_mul(MIB));
                let page_size = host
                    .command_u64("/usr/sbin/sysctl", &["-n", "hw.pagesize"])
                    .unwrap_or(16_384);
                (total_memory, page_size, metal)
            }
            // Linux (and other non-macOS targets): read from /proc and /sys.
            Platform::Linux => (
                linux_total_memory(host.as_ref()),
                linux_page_size(host.as_ref()),
                linux_metal_init(host.as_ref()),
            ),
        };
        Self {
            llm_client: LlmTelemetryClient::from_config(&config, home.clone()),
            host,
            platform,
            home,
            page_size,
            total_memory,
            metal,
            seen_requests: VecDeque::new(),
            correlation: CorrelationEngine::default(),
            previous: None,
            previous_llm_cache: None,
            current: Sample::default(),
            generation_history: VecDeque::with_capacity(history_limit),
            prefill_history: VecDeque::with_capacity(history_limit),
            cache_history: VecDeque::with_capacity(history_limit),
            load_history: VecDeque::with_capacity(history_limit),
            swap_history: VecDeque::with_capacity(history_limit),
            gpu_history: VecDeque::with_capacity(history_limit),
            signals: VecDeque::with_capacity(8),
            request_history: request_dashboard::History::default(),
            operator_history: operator_charts::History::default(),
            history_limit,
            thresholds: Thresholds::from_config(&config),
        }
    }

    fn sample(&mut self) -> Sample {
        let sample_started = Instant::now();
        let now = sample_started;
        let mut sample = Sample {
            total_memory: self.total_memory,
            metal: self.metal.clone(),
            ..Sample::default()
        };

        let host = self.host.as_ref();
        match self.platform {
            Platform::MacOs => sample_macos_memory(host, &mut sample, self.page_size),
            Platform::Linux => sample_linux_memory(
                host,
                &mut sample,
                self.page_size,
                self.total_memory,
                self.thresholds,
            ),
        }
        let counters = match self.platform {
            Platform::MacOs => macos_counters_for_rates(host, self.page_size),
            Platform::Linux => linux_counters_for_rates(host),
        };
        let process_elapsed = self
            .previous
            .as_ref()
            .map(|previous| now.duration_since(previous.at));

        if let Some(previous) = &self.previous {
            let elapsed = now.duration_since(previous.at);
            sample.swap_in = rate_bytes(
                delta(counters.swapins, previous.swapins),
                self.page_size,
                elapsed,
            );
            sample.swap_out = rate_bytes(
                delta(counters.swapouts, previous.swapouts),
                self.page_size,
                elapsed,
            );
            sample.compress = rate_bytes(
                delta(counters.compressions, previous.compressions),
                self.page_size,
                elapsed,
            );
            sample.decompress = rate_bytes(
                delta(counters.decompressions, previous.decompressions),
                self.page_size,
                elapsed,
            );
            sample.reactivated = rate_bytes(
                delta(counters.reactivations, previous.reactivations),
                self.page_size,
                elapsed,
            );
            sample.swap_growth = signed_rate_bytes(sample.swap_used, previous.swap_used, elapsed);
            sample.rate_ready = true;
        }
        self.previous = Some(PreviousCounters {
            at: now,
            swapins: counters.swapins,
            swapouts: counters.swapouts,
            compressions: counters.compressions,
            decompressions: counters.decompressions,
            reactivations: counters.reactivations,
            swap_used: sample.swap_used,
        });

        match self.platform {
            Platform::MacOs => sample_macos_gpu_thermal(host, &mut sample),
            Platform::Linux => sample_linux_gpu_thermal(host, &mut sample, &self.current.gpus),
        }

        let process_snapshot = match self.platform {
            Platform::MacOs => parse_processes(
                &host
                    .command(
                        "/bin/ps",
                        &["-axo", "pid=,rss=,%cpu=,%mem=,state=,pagein=,comm=,args="],
                    )
                    .unwrap_or_default(),
            ),
            // Linux `ps` has no `pagein` column; `maj_flt` (major faults)
            // keeps the same positional layout for the shared parser.
            Platform::Linux => parse_processes(
                &host
                    .command(
                        "ps",
                        &["-axo", "pid=,rss=,%cpu=,%mem=,stat=,maj_flt=,comm=,args="],
                    )
                    .unwrap_or_default(),
            ),
        };
        sample.llm_count = process_snapshot.llm_count;
        sample.llm_rss = process_snapshot.llm_rss;
        sample.llm_cpu = process_snapshot.llm_cpu;
        let detected_provider = process_snapshot.provider.clone();
        sample.llm_pid = process_snapshot
            .top_llm
            .as_ref()
            .map(|process| process.pid)
            .unwrap_or_default();
        sample.process_memory = process_memory::read(sample.llm_pid);
        sample.process_memory_growth = sample.process_memory.as_ref().and_then(|reading| {
            process_memory::growth(reading, self.current.process_memory.as_ref())
        });
        sample.llm_top = process_snapshot
            .top_llm
            .as_ref()
            .map(|process| process.name.clone())
            .unwrap_or_else(|| "none".into());
        sample.largest_consumer = process_snapshot.largest_consumer;
        let mut llm_processes = process_snapshot.llm_processes;
        if let Some(elapsed) = process_elapsed {
            annotate_process_pagein_rates(&mut llm_processes, &self.current.llm_processes, elapsed);
        }
        sample.llm_processes = llm_processes;
        let live_stats = self.llm_client.poll(detected_provider.as_deref());
        let should_read_log = !self
            .llm_client
            .provider_adapter
            .selected(detected_provider.as_deref())
            && live_stats.is_none()
            && detected_provider
                .as_deref()
                .map(|provider| provider == "oMLX")
                .unwrap_or(true);
        let log_stats = if should_read_log {
            read_llm_stats(self.home.as_deref())
        } else {
            LlmLogStats::default()
        };
        let llm_stats = live_stats.as_ref();
        sample.llm_remote = llm_stats.is_some_and(|stats| stats.remote);
        sample.llm_details = llm_stats.and_then(|stats| stats.details.clone());
        sample.mlx = llm_stats.map(|stats| stats.mlx.clone()).unwrap_or_default();
        sample.metal.resource_limit = sample.metal.resource_limit.or(sample.mlx.resource_limit);
        sample.llm_requests = llm_stats
            .map(|stats| stats.requests.clone())
            .unwrap_or_default();
        let live_is_stale = llm_stats
            .filter(|stats| stats.source == TelemetrySource::Live)
            .and_then(|stats| stats.observed_at)
            .and_then(|observed_at| SystemTime::now().duration_since(observed_at).ok())
            .is_some_and(|age| age > Duration::from_secs(5));
        let use_omlx_log = log_stats.model.is_some()
            && detected_provider
                .as_deref()
                .map(|provider| provider == "oMLX")
                .unwrap_or(true);
        sample.llm_provider = llm_stats
            .and_then(|stats| stats.provider.clone())
            .unwrap_or_else(|| {
                if let Some(provider) = self.llm_client.provider_adapter.provider() {
                    provider.to_owned()
                } else if use_omlx_log {
                    "oMLX".into()
                } else {
                    "none".into()
                }
            });
        sample.llm_status = if live_is_stale {
            "stale".into()
        } else {
            llm_stats
                .and_then(|stats| stats.status.clone())
                .unwrap_or_else(|| {
                    if use_omlx_log {
                        "last result".into()
                    } else if sample.llm_count > 0 {
                        "running".into()
                    } else {
                        "offline".into()
                    }
                })
        };
        sample.llm_model = llm_stats
            .and_then(|stats| stats.model.clone())
            .or_else(|| use_omlx_log.then(|| log_stats.model.clone()).flatten())
            .unwrap_or_else(|| {
                if sample.llm_count > 0 {
                    sample.llm_top.clone()
                } else {
                    "not detected".into()
                }
            });
        sample.llm_source = live_stats
            .as_ref()
            .map(|stats| stats.source)
            .unwrap_or_else(|| {
                if use_omlx_log {
                    TelemetrySource::Log
                } else {
                    TelemetrySource::None
                }
            });
        sample.llm_observed_at = live_stats
            .as_ref()
            .and_then(|stats| stats.observed_at)
            .or_else(|| use_omlx_log.then_some(log_stats.observed_at).flatten());
        sample.llm_generation_tps =
            llm_stats
                .and_then(|stats| stats.generation_tps)
                .or_else(|| {
                    if live_stats.is_none() && use_omlx_log {
                        log_stats.tokens_per_second
                    } else {
                        None
                    }
                });
        sample.llm_generation_tps_live = !live_is_stale
            && llm_stats
                .map(|stats| stats.generation_tps_live)
                .unwrap_or(false);
        sample.llm_prefill_tps = llm_stats.and_then(|stats| stats.prefill_tps);
        sample.llm_prefill_tps_live = !live_is_stale
            && llm_stats
                .map(|stats| stats.prefill_tps_live)
                .unwrap_or(false);
        sample.llm_output_tokens = llm_stats.and_then(|stats| stats.output_tokens).or_else(|| {
            if live_stats.is_none() && use_omlx_log {
                log_stats.output_tokens
            } else {
                None
            }
        });
        sample.llm_prompt_tokens = llm_stats.and_then(|stats| stats.prompt_tokens).or_else(|| {
            if live_stats.is_none() && use_omlx_log {
                log_stats.prompt_tokens
            } else {
                None
            }
        });
        sample.llm_cache_efficiency = llm_stats.and_then(|stats| stats.cache_efficiency);
        sample.llm_cache_interval_efficiency =
            cache_interval_efficiency(&mut self.previous_llm_cache, llm_stats, live_is_stale)
                .or_else(|| {
                    (!live_is_stale)
                        .then(|| llm_stats.and_then(|s| s.cache_interval_efficiency))
                        .flatten()
                });
        sample.llm_prefix_hit_rate = llm_stats.and_then(|stats| stats.prefix_hit_rate);
        sample.llm_active_requests = llm_stats.and_then(|stats| stats.active_requests);
        sample.llm_waiting_requests = llm_stats.and_then(|stats| stats.waiting_requests);
        sample.llm_model_memory = llm_stats.and_then(|stats| stats.model_memory);
        sample.llm_model_memory_max = llm_stats.and_then(|stats| stats.model_memory_max);
        sample.updated = now_clock(self.host.as_ref());
        let previous = if self.current.updated == "waiting" {
            None
        } else {
            Some(self.current.clone())
        };
        sample.correlation = self.correlation.observe(&sample, self.thresholds);
        classify(&mut sample, previous.as_ref(), self.thresholds);

        if previous.as_ref().is_some_and(|old| {
            old.llm_provider != sample.llm_provider || old.llm_model != sample.llm_model
        }) {
            self.generation_history.clear();
            self.prefill_history.clear();
        }

        push_history(
            &mut self.generation_history,
            chart_rate_value(&sample, ChartMetric::Generation),
            ChartMetric::Generation,
            self.history_limit,
            self.thresholds,
        );
        push_history(
            &mut self.prefill_history,
            chart_rate_value(&sample, ChartMetric::Prefill),
            ChartMetric::Prefill,
            self.history_limit,
            self.thresholds,
        );
        push_history(
            &mut self.cache_history,
            sample
                .llm_cache_interval_efficiency
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value.round() as u64),
            ChartMetric::Cache,
            self.history_limit,
            self.thresholds,
        );
        if let Some(point) = self.generation_history.back_mut() {
            point.tone = if point.value.is_none() {
                Tone::Muted
            } else if sample.correlation.is_material_drop() {
                sample.correlation.tone()
            } else {
                Tone::Cyan
            };
        }
        push_history_with_tone(
            &mut self.load_history,
            resident_memory_percent(&sample),
            sample.pressure_tone,
            self.history_limit,
        );
        push_history(
            &mut self.swap_history,
            (sample.swap_available && sample.rate_ready)
                .then_some(sample.swap_in.saturating_add(sample.swap_out)),
            ChartMetric::Swap,
            self.history_limit,
            self.thresholds,
        );
        push_history(
            &mut self.gpu_history,
            sample.gpu_util.map(u64::from),
            ChartMetric::Gpu,
            self.history_limit,
            self.thresholds,
        );

        self.record_journal_events(previous.as_ref(), &sample);
        diagnostics_log(
            "INFO",
            "sample",
            format!(
                "duration_ms={} status={} provider={} model={} source={} gen_live={} gen_tps={} prefill_live={} prefill_tps={} cache={} active={} waiting={} gpu={} renderer={} tiler={} ram_resident_percent={} vm_availability={} paging_in={} paging_out={} compress={} decompress={}",
                sample_started.elapsed().as_millis(),
                log_field(&sample.llm_status),
                log_field(&sample.llm_provider),
                log_field(&sample.llm_model),
                log_field(sample.llm_source.label()),
                sample.llm_generation_tps_live,
                log_optional_f64(sample.llm_generation_tps),
                sample.llm_prefill_tps_live,
                log_optional_f64(sample.llm_prefill_tps),
                log_optional_f64(sample.llm_cache_efficiency),
                log_optional_u64(sample.llm_active_requests),
                log_optional_u64(sample.llm_waiting_requests),
                log_optional_u8(sample.gpu_util),
                log_optional_u8(sample.metal.renderer_util),
                log_optional_u8(sample.metal.tiler_util),
                log_optional_u64(resident_memory_percent(&sample)),
                log_optional_u8(sample.availability),
                sample.swap_in,
                sample.swap_out,
                sample.compress,
                sample.decompress,
            ),
        );
        self.current = sample.clone();
        sample
    }

    fn view(&self) -> CollectorView {
        CollectorView {
            current: self.current.clone(),
            generation_history: self.generation_history.clone(),
            prefill_history: self.prefill_history.clone(),
            cache_history: self.cache_history.clone(),
            load_history: self.load_history.clone(),
            swap_history: self.swap_history.clone(),
            gpu_history: self.gpu_history.clone(),
            signals: self.signals.clone(),
            request_history: self.request_history.clone(),
            operator_history: self.operator_history.clone(),
        }
    }

    fn record_journal_events(&mut self, previous: Option<&Sample>, sample: &Sample) {
        let mut events = Vec::new();
        let mut add = |state: &str, summary: String, tone: Tone| {
            events.push((state.to_string(), summary, tone));
        };

        self.request_history.observe(&sample.llm_requests);
        self.operator_history.observe(sample, self.history_limit);
        for request in &sample.llm_requests {
            if let Some(summary) = providers::new_request_summary(&mut self.seen_requests, request)
            {
                add("PROMPT", summary, Tone::Cyan);
            }
        }

        if let Some(previous) = previous {
            if previous.impact != sample.impact && sample.impact != "SAMPLING" {
                add(&sample.impact, signal_summary(sample), sample.impact_tone);
            }

            if (previous.llm_provider != sample.llm_provider
                || previous.llm_model != sample.llm_model)
                && sample.llm_provider != "none"
                && sample.llm_model != "not detected"
            {
                add(
                    "LLM",
                    format!(
                        "detected {} · {}",
                        sample.llm_provider,
                        llm_model_label(sample, 42)
                    ),
                    Tone::Cyan,
                );
            }

            if previous.llm_status != sample.llm_status {
                add(
                    "LLM",
                    format!(
                        "status {} → {} · {}",
                        previous.llm_status, sample.llm_status, sample.llm_provider
                    ),
                    Tone::Cyan,
                );
            }

            if previous.llm_source != sample.llm_source {
                add(
                    "LLM",
                    format!(
                        "telemetry source · {} → {}",
                        previous.llm_source.label(),
                        sample.llm_source.label()
                    ),
                    if sample.llm_source == TelemetrySource::Live {
                        Tone::Green
                    } else {
                        Tone::Yellow
                    },
                );
            }

            let previous_active = previous.llm_active_requests.unwrap_or(0);
            let active = sample.llm_active_requests.unwrap_or(0);
            if (previous_active == 0) != (active == 0) {
                add(
                    "LLM",
                    if active == 0 {
                        "request completed · serving is idle".into()
                    } else {
                        format!("request started · {active} active")
                    },
                    Tone::Cyan,
                );
            }

            let previous_waiting = previous.llm_waiting_requests.unwrap_or(0);
            let waiting = sample.llm_waiting_requests.unwrap_or(0);
            if (previous_waiting == 0) != (waiting == 0) {
                add(
                    "QUEUE",
                    if waiting == 0 {
                        "queue cleared".into()
                    } else {
                        format!("{waiting} request(s) waiting")
                    },
                    if waiting == 0 {
                        Tone::Green
                    } else {
                        Tone::Yellow
                    },
                );
            }

            if previous.llm_generation_tps.is_none() && sample.llm_generation_tps.is_some() {
                add(
                    "LLM",
                    format!(
                        "{} telemetry online · {} · {}",
                        sample.llm_source.label(),
                        llm_generation_rate_label(sample),
                        llm_prefill_rate_label(sample)
                    ),
                    Tone::Green,
                );
            }

            if sample.correlation.is_material_drop()
                && previous.correlation.event_key != sample.correlation.event_key
            {
                add(
                    "LLM",
                    format!(
                        "throughput diagnosis · {} · {} confidence",
                        sample.correlation.summary,
                        sample.correlation.confidence_label()
                    ),
                    sample.correlation.tone(),
                );
            }

            if previous.pressure != sample.pressure && !sample.pressure.is_empty() {
                add(
                    "PRESSURE",
                    format!("memory state {}", pressure_state_label(sample)),
                    sample.pressure_tone,
                );
            }

            let previous_paging = previous.swap_in.saturating_add(previous.swap_out) > 0;
            let paging = sample.swap_in.saturating_add(sample.swap_out) > 0;
            if previous_paging != paging {
                add(
                    "PAGING",
                    if paging {
                        format!(
                            "active · in {} · out {}",
                            rate(sample.swap_in),
                            rate(sample.swap_out)
                        )
                    } else {
                        "cleared · no current paging traffic".into()
                    },
                    if paging { Tone::Yellow } else { Tone::Green },
                );
            }

            if let Some((summary, tone)) =
                gpu_journal_transition(previous.gpu_util, sample.gpu_util, self.thresholds)
            {
                add("GPU", summary, tone);
            }

            if previous.thermal != sample.thermal
                && sample.thermal != "unavailable"
                && !sample.thermal.ends_with("°C measured")
            {
                add(
                    "THERMAL",
                    sample.thermal.clone(),
                    if sample.thermal == "no warning" {
                        Tone::Green
                    } else {
                        Tone::Yellow
                    },
                );
            }
        } else {
            add(
                "SYSTEM",
                format!(
                    "journal started · RAM resident {} · pressure {}",
                    sample
                        .resident_memory
                        .map(bytes)
                        .unwrap_or_else(|| "—".into()),
                    pressure_state_label(sample)
                ),
                Tone::Cyan,
            );
            if sample.llm_provider != "none" && sample.llm_model != "not detected" {
                add(
                    "LLM",
                    format!(
                        "detected {} · {}",
                        sample.llm_provider,
                        llm_model_label(sample, 42)
                    ),
                    Tone::Cyan,
                );
            }
        }

        for (state, summary, tone) in events {
            self.signals.push_back(SignalEvent {
                time: sample.updated.clone(),
                recorded_at: SystemTime::now(),
                kind: EventKind::from_state(&state),
                state,
                summary,
                tone,
            });
        }
        let journal_limit = self.history_limit.clamp(40, 240);
        while self.signals.len() > journal_limit {
            self.signals.pop_front();
        }
    }

    fn reset(&mut self) {
        self.previous = None;
        self.previous_llm_cache = None;
        self.seen_requests.clear();
        self.request_history = request_dashboard::History::default();
        self.operator_history = operator_charts::History::default();
        self.correlation.reset();
        self.generation_history.clear();
        self.prefill_history.clear();
        self.cache_history.clear();
        self.load_history.clear();
        self.swap_history.clear();
        self.gpu_history.clear();
        self.signals.clear();
        self.current = Sample::default();
    }
}

/// A raised aggressive-paging alert. It survives until the user acknowledges
/// it or the paging episode ends, whichever comes first.
struct ActiveAlert {
    state: String,
    summary: String,
    time: String,
}

/// Critical host conditions, deliberately excluding GPU utilization.
fn is_critical_state(state: &str) -> bool {
    matches!(
        state,
        "MEMORY BOTTLENECK" | "SWAP THRASHING" | "HEAVY PAGING" | "PAGE-IN RECOVERY"
    )
}

fn critical_state(sample: &Sample) -> Option<&str> {
    // A reported critical memory condition is valid even if another counter
    // is unavailable and the overall classifier says DATA LIMITED.
    if sample.pressure == "RED" {
        Some("MEMORY BOTTLENECK")
    } else {
        is_critical_state(&sample.impact).then_some(sample.impact.as_str())
    }
}

fn critical_summary(sample: &Sample) -> String {
    if critical_state(sample) == Some("MEMORY BOTTLENECK") {
        format!(
            "critical memory pressure · RAM resident {}",
            sample
                .resident_memory
                .map(bytes)
                .unwrap_or_else(|| "—".into())
        )
    } else {
        signal_summary(sample)
    }
}

/// BEL passes through the alternate screen to the terminal emulator, so the
/// user's audible/visual bell setting decides how the alert sounds. Called
/// between frames only: writing mid-draw could interleave with the buffer.
fn ring_terminal_bell() {
    write_terminal_bell(&mut stdout());
}

fn write_terminal_bell(out: &mut impl Write) {
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

struct App {
    collector: CollectorView,
    sampler: Sampler,
    interval: Duration,
    paused: bool,
    tab: usize,
    top_sort: TopSort,
    top_filter: String,
    top_filtering: bool,
    top_selected: usize,
    journal_filter: JournalFilter,
    journal_scroll: usize,
    request_scroll: usize,
    charts: chart_navigation::Navigation,
    gpu_selected: usize,
    help: bool,
    quit: bool,
    sampler_disconnected: bool,
    alert: Option<ActiveAlert>,
    alert_bells: usize,
    /// Rings the terminal bell; replaced in tests so no BEL reaches stdout.
    bell: fn(),
    critical_episode: bool,
    thresholds: Thresholds,
}

impl App {
    fn new(interval: u64, history: usize, config: Config) -> Self {
        let sampler = Sampler::spawn(Duration::from_secs(interval), history, config.clone());
        Self::with_sampler(interval, history, config, sampler)
    }

    fn with_sampler(interval: u64, _history: usize, config: Config, sampler: Sampler) -> Self {
        let interval = Duration::from_secs(interval);
        let thresholds = Thresholds::from_config(&config);
        Self {
            collector: CollectorView {
                current: Sample::default(),
                generation_history: VecDeque::new(),
                prefill_history: VecDeque::new(),
                cache_history: VecDeque::new(),
                load_history: VecDeque::new(),
                swap_history: VecDeque::new(),
                gpu_history: VecDeque::new(),
                signals: VecDeque::new(),
                request_history: request_dashboard::History::default(),
                operator_history: operator_charts::History::default(),
            },
            sampler,
            interval,
            paused: false,
            tab: 0,
            top_sort: TopSort::Rss,
            top_filter: String::new(),
            top_filtering: false,
            top_selected: 0,
            journal_filter: JournalFilter::All,
            journal_scroll: 0,
            request_scroll: 0,
            charts: chart_navigation::Navigation::default(),
            gpu_selected: 0,
            help: false,
            quit: false,
            sampler_disconnected: false,
            alert: None,
            alert_bells: 0,
            bell: ring_terminal_bell,
            critical_episode: false,
            thresholds,
        }
    }

    fn tick(&mut self) {
        loop {
            match self.sampler.views.try_recv() {
                Ok(view) => {
                    self.track_critical_alert(&view);
                    self.gpu_selected = self
                        .collector
                        .current
                        .gpus
                        .get(self.gpu_selected)
                        .and_then(|selected| {
                            view.current
                                .gpus
                                .iter()
                                .position(|gpu| gpu.uuid == selected.uuid)
                        })
                        .unwrap_or_else(|| {
                            self.gpu_selected
                                .min(view.current.gpus.len().saturating_sub(1))
                        });
                    self.collector = view;
                    self.sampler_disconnected = false;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.sampler_disconnected {
                        diagnostics_log(
                            "ERROR",
                            "sampler_disconnected",
                            "no_more_samples_received",
                        );
                        self.sampler_disconnected = true;
                    }
                    break;
                }
            }
        }
    }

    /// Ring once per critical episode. Acknowledgment silences the episode;
    /// recovery re-arms it. Escalation refreshes the banner without repeating.
    fn track_critical_alert(&mut self, view: &CollectorView) {
        let state = critical_state(&view.current);
        if state.is_none() && matches!(view.current.impact.as_str(), "SAMPLING" | "DATA LIMITED") {
            return;
        }
        let newly_critical = state.is_some() && !self.critical_episode;
        self.critical_episode = state.is_some();
        if let Some(alert) = &mut self.alert {
            if let Some(state) = state {
                if alert.state != state {
                    alert.time = view.current.updated.clone();
                }
                alert.state = state.into();
                alert.summary = critical_summary(&view.current);
            } else {
                diagnostics_log(
                    "INFO",
                    "critical_alert_cleared",
                    format!("state={}", log_field(&alert.state)),
                );
                self.alert = None;
            }
            return;
        }
        if let Some(state) = state.filter(|_| newly_critical) {
            let summary = critical_summary(&view.current);
            diagnostics_log(
                "WARN",
                "critical_alert_raised",
                format!("state={} summary={}", log_field(state), log_field(&summary)),
            );
            (self.bell)();
            self.alert_bells += 1;
            self.alert = Some(ActiveAlert {
                state: state.into(),
                summary,
                time: view.current.updated.clone(),
            });
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.help {
            if matches!(
                key.code,
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('h')
            ) {
                self.help = false;
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        // View navigation stays global, including while editing a process filter.
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.top_filtering = false;
            self.charts.expanded = false;
            self.handle_global_key(key);
            return;
        }
        if self.top_filtering {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.top_filtering = false,
                KeyCode::Backspace => {
                    self.top_filter.pop();
                }
                KeyCode::Char(value)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.top_filter.push(value);
                }
                _ => {}
            }
            return;
        }
        if self.tab == 1 {
            match key.code {
                KeyCode::Up => {
                    self.top_selected = self.top_selected.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    self.top_selected = self.top_selected.saturating_add(1);
                    return;
                }
                KeyCode::PageUp => {
                    self.top_selected = self.top_selected.saturating_sub(10);
                    return;
                }
                KeyCode::PageDown => {
                    self.top_selected = self.top_selected.saturating_add(10);
                    return;
                }
                KeyCode::Home => {
                    self.top_selected = 0;
                    return;
                }
                KeyCode::End => {
                    self.top_selected = self.filtered_llm_processes().len().saturating_sub(1);
                    return;
                }
                KeyCode::Char('s') => {
                    self.top_sort = self.top_sort.next();
                    self.top_selected = 0;
                    return;
                }
                KeyCode::Char('f') | KeyCode::Char('/') => {
                    self.top_filtering = true;
                    return;
                }
                KeyCode::Char('c') => {
                    self.top_filter.clear();
                    self.top_selected = 0;
                    return;
                }
                _ => {}
            }
        }
        if self.tab == 0 {
            let last = self.collector.request_history.len().saturating_sub(1);
            match key.code {
                KeyCode::Char('[') => self.gpu_selected = self.gpu_selected.saturating_sub(1),
                KeyCode::Char(']') => {
                    self.gpu_selected = self
                        .gpu_selected
                        .saturating_add(1)
                        .min(self.collector.current.gpus.len().saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.charts.focused = Chart::Prompt;
                    self.request_scroll = if key.code == KeyCode::Up {
                        self.request_scroll.saturating_sub(1)
                    } else {
                        self.request_scroll.saturating_add(1).min(last)
                    };
                }
                direction @ (KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right) => {
                    if self.charts.expanded {
                        self.charts
                            .cycle(matches!(direction, KeyCode::Up | KeyCode::Left));
                    } else {
                        self.charts.move_focus(direction);
                    }
                }
                KeyCode::Char('+') | KeyCode::Char('=') => self.charts.change_zoom(true),
                KeyCode::Char('-') => self.charts.change_zoom(false),
                KeyCode::Char('0') => self.charts.reset_zoom(),
                KeyCode::Enter => self.charts.expanded = !self.charts.expanded,
                KeyCode::Esc if self.charts.expanded => self.charts.expanded = false,
                KeyCode::PageUp => self.request_scroll = self.request_scroll.saturating_sub(10),
                KeyCode::PageDown => {
                    self.request_scroll = self.request_scroll.saturating_add(10).min(last)
                }
                KeyCode::Home => self.request_scroll = 0,
                KeyCode::End => self.request_scroll = last,
                _ => {
                    self.handle_global_key(key);
                    return;
                }
            }
            return;
        }
        if self.tab == 2 {
            match key.code {
                // The journal is newest-first: Up returns toward the live edge,
                // Down moves toward older records.
                KeyCode::Up => {
                    self.journal_scroll = self.journal_scroll.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    self.journal_scroll = self.journal_scroll.saturating_add(1);
                    return;
                }
                KeyCode::PageUp => {
                    self.journal_scroll = self.journal_scroll.saturating_sub(10);
                    return;
                }
                KeyCode::PageDown => {
                    self.journal_scroll = self.journal_scroll.saturating_add(10);
                    return;
                }
                KeyCode::Home => {
                    self.journal_scroll = 0;
                    return;
                }
                KeyCode::End => {
                    self.journal_scroll = self.filtered_journal_events().len().saturating_sub(1);
                    return;
                }
                KeyCode::Char('f') | KeyCode::Char(']') => {
                    self.journal_filter = self.journal_filter.next();
                    self.journal_scroll = 0;
                    return;
                }
                KeyCode::Char('[') => {
                    self.journal_filter = self.journal_filter.previous();
                    self.journal_scroll = 0;
                    return;
                }
                _ => {}
            }
        }
        self.handle_global_key(key);
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.help || self.tab != 0 {
            return;
        }
        let Some(chart) = self.charts.at(mouse.column, mouse.row) else {
            return;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.charts.focused = chart,
            MouseEventKind::Down(MouseButton::Right) => {
                self.charts.focused = chart;
                self.charts.expanded = !self.charts.expanded;
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.charts.focused = chart;
                self.charts
                    .change_zoom(mouse.kind == MouseEventKind::ScrollUp);
            }
            _ => {}
        }
    }

    fn handle_global_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('a') => {
                if let Some(alert) = self.alert.take() {
                    diagnostics_log(
                        "INFO",
                        "critical_alert_acknowledged",
                        format!("state={}", log_field(&alert.state)),
                    );
                }
            }
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                self.paused = !self.paused;
                self.sampler.send(SamplerCommand::SetPaused(self.paused));
            }
            KeyCode::Char('r') => {
                self.sampler.send(SamplerCommand::Reset);
                self.journal_scroll = 0;
                self.request_scroll = 0;
                self.charts = chart_navigation::Navigation::default();
                self.gpu_selected = 0;
                self.journal_filter = JournalFilter::All;
                self.alert = None;
                self.critical_episode = false;
            }
            KeyCode::Char('t') | KeyCode::Char('2') => {
                self.tab = 1;
                self.top_selected = 0;
            }
            KeyCode::Char('j') | KeyCode::Char('3') => {
                self.tab = 2;
                self.journal_scroll = 0;
            }
            KeyCode::Char('o') | KeyCode::Char('1') => self.tab = 0,
            KeyCode::Tab | KeyCode::Right => self.tab = (self.tab + 1) % 3,
            KeyCode::BackTab | KeyCode::Left => self.tab = (self.tab + 2) % 3,
            KeyCode::Char('?') | KeyCode::Char('h') => self.help = true,
            KeyCode::Char('}') => {
                let seconds = self.interval.as_secs().saturating_add(1).min(60);
                self.interval = Duration::from_secs(seconds);
                self.sampler
                    .send(SamplerCommand::SetInterval(self.interval));
            }
            KeyCode::Char('{') => {
                let seconds = self.interval.as_secs().saturating_sub(1).max(1);
                self.interval = Duration::from_secs(seconds);
                self.sampler
                    .send(SamplerCommand::SetInterval(self.interval));
            }
            _ => {}
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        self.charts.regions.borrow_mut().clear();
        self.charts.overview_samples.set(None);
        if area.width < 72 || area.height < 24 {
            self.draw_compact_warning(frame, area);
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(Color::Rgb(10, 14, 21))),
            area,
        );
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(area);
        self.draw_header(frame, outer[0]);
        self.draw_controls(frame, outer[2]);
        match self.tab {
            0 => self.draw_overview(frame, outer[1]),
            1 => self.draw_llm_top(frame, outer[1]),
            _ => self.draw_journal(frame, outer[1]),
        }
        if self.tab == 0 {
            if self.charts.expanded {
                frame.render_widget(Clear, outer[1]);
                self.charts.overview_samples.set(None);
                self.draw_selected_chart(frame, outer[1]);
            }
            self.charts.decorate(frame);
        }
        if self.alert.is_some() {
            self.draw_alert_banner(frame, outer[1]);
        }
        if self.help {
            self.draw_help(frame, area);
        }
    }

    /// Overlay strip at the top of the tab content: a critical condition demands
    /// attention without stealing a permanent layout row from the panels.
    fn draw_alert_banner(&self, frame: &mut Frame, area: Rect) {
        let Some(alert) = &self.alert else {
            return;
        };
        let height = area.height.min(3);
        if height == 0 || area.width < 20 {
            return;
        }
        let banner = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height,
        };
        frame.render_widget(Clear, banner);
        let text = Line::from(vec![
            Span::styled(
                format!(" ⚠ {} ", alert.state),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                alert.summary.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ·  raised {}  ·  a acknowledge", alert.time),
                Style::default().fg(MUTED),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(RED))
                    .style(Style::default().bg(PANEL)),
            ),
            banner,
        );
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let compact_tabs = area.width < 96;
        let tab_labels = if compact_tabs {
            vec![
                Line::from("1 OVR"),
                Line::from("2 TOP"),
                Line::from("3 JRN"),
            ]
        } else {
            vec![
                Line::from("1 Overview"),
                Line::from("2 MLX Top"),
                Line::from("3 Journal"),
            ]
        };
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(24),
                Constraint::Length(if compact_tabs { 30 } else { 42 }),
                Constraint::Fill(1),
            ])
            .split(area);
        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                " mlxtop ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{VERSION}"),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
        ]));
        frame.render_widget(title, chunks[0]);

        let tabs = Tabs::new(tab_labels)
            .select(self.tab)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(CYAN)
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().fg(MUTED))
            .divider(Span::styled(" · ", Style::default().fg(DIM)));
        frame.render_widget(tabs, chunks[1]);
        frame.render_widget(
            Paragraph::new(format!(
                "{} · {}s ",
                if self.paused { "PAUSED" } else { "SAMPLING" },
                self.interval.as_secs()
            ))
            .alignment(Alignment::Right)
            .style(Style::default().fg(if self.paused { YELLOW } else { MUTED })),
            chunks[2],
        );
    }

    fn draw_request_chart(&self, frame: &mut Frame, area: Rect) {
        self.charts.register(Chart::Prompt, area);
        request_dashboard::draw(
            frame,
            area,
            &self.collector.request_history,
            &self.collector.current,
            self.request_scroll,
            self.charts.zoom(Chart::Prompt),
        );
    }

    fn draw_operator_chart(&self, frame: &mut Frame, area: Rect, chart: Chart) {
        self.charts.register(chart, area);
        let zoom = self.charts.zoom(chart);
        match chart {
            Chart::Queue => operator_charts::queue(
                frame,
                area,
                &self.collector.operator_history,
                self.interval,
                zoom,
                self.charts.overview_samples.get(),
            ),
            Chart::Latency => {
                operator_charts::latency(frame, area, &self.collector.operator_history, zoom)
            }
            _ => unreachable!("operator chart expected"),
        }
    }

    fn draw_selected_chart(&self, frame: &mut Frame, area: Rect) {
        match self.charts.focused {
            Chart::Prompt => self.draw_request_chart(frame, area),
            chart @ (Chart::Queue | Chart::Latency) => self.draw_operator_chart(frame, area, chart),
            chart => {
                let (name, history, metric) = match chart {
                    Chart::Generation => (
                        "generation",
                        &self.collector.generation_history,
                        ChartMetric::Generation,
                    ),
                    Chart::Prefill => (
                        "prefill",
                        &self.collector.prefill_history,
                        ChartMetric::Prefill,
                    ),
                    Chart::Cache => ("cache", &self.collector.cache_history, ChartMetric::Cache),
                    Chart::Gpu => (
                        self.gpu_chart_title(),
                        &self.collector.gpu_history,
                        ChartMetric::Gpu,
                    ),
                    Chart::Memory => ("memory", &self.collector.load_history, ChartMetric::Memory),
                    Chart::Paging => ("paging", &self.collector.swap_history, ChartMetric::Swap),
                    _ => unreachable!("indicator chart expected"),
                };
                self.render_indicator_chart(frame, area, name, history, metric);
            }
        }
    }

    fn draw_overview(&self, frame: &mut Frame, area: Rect) {
        if self.collector.current.has_nvidia_gpus() {
            self.draw_gpu_overview(frame, area);
        } else {
            self.draw_overview_layout(frame, area, false);
        }
    }

    fn draw_gpu_overview(&self, frame: &mut Frame, area: Rect) {
        self.draw_overview_layout(frame, area, true);
    }

    fn draw_overview_layout(&self, frame: &mut Frame, area: Rect, devices: bool) {
        let compact = area.height < 30;
        let info_height = 3;
        let device_height = if devices {
            gpu_dashboard::height(
                self.collector.current.gpus.len(),
                if compact { 4 } else { 6 },
            )
        } else {
            0
        };
        // Keep idle geometry stable, but give throughput and request bars enough
        // vertical resolution. Resources remain visible in a compact first row.
        let journal_height = if compact && devices {
            0
        } else if compact {
            3
        } else {
            7
        };
        let request_height = if compact {
            if devices {
                6
            } else {
                7
            }
        } else {
            (area.height / 4).clamp(9, 13)
        };
        let remaining = area
            .height
            .saturating_sub(info_height + device_height + journal_height + request_height);
        let resource_height = (remaining / 2).clamp(4, 8).min(remaining);
        let rates_height = remaining.saturating_sub(resource_height);
        // Fit every captured observation into even the narrowest plot, then
        // widen those same observations across larger panels. No decimation.
        let resources =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
        let rates = Layout::horizontal([
            Constraint::Percentage(40),
            Constraint::Percentage(35),
            Constraint::Percentage(25),
        ])
        .split(area);
        let supporting = Layout::horizontal([
            Constraint::Percentage(50),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);
        let queue_width = operator_charts::queue_plot_width(
            &self.collector.operator_history,
            supporting[2].width,
        );
        let samples = [
            resources[0].width.saturating_sub(7),
            resources[1].width.saturating_sub(14),
            rates[0].width.saturating_sub(8),
            rates[1].width.saturating_sub(8),
            rates[2].width.saturating_sub(7),
            supporting[1].width.saturating_sub(7),
            queue_width,
        ]
        .into_iter()
        .min()
        .unwrap_or(1)
        .max(1);
        self.charts.overview_samples.set(Some(usize::from(samples)));
        let rows = Layout::vertical([
            Constraint::Length(info_height),
            Constraint::Length(device_height),
            Constraint::Length(resource_height),
            Constraint::Length(rates_height),
            Constraint::Length(request_height),
            Constraint::Length(journal_height),
        ])
        .split(area);
        model_dashboard::draw(
            frame,
            rows[0],
            &self.collector.current,
            &self.collector.request_history,
        );
        if devices {
            gpu_dashboard::draw(
                frame,
                rows[1],
                &self.collector.current.gpus,
                self.gpu_selected,
                self.thresholds,
            );
        }
        self.draw_resource_charts(frame, rows[2]);
        self.draw_workload_charts(frame, rows[3]);
        self.draw_supporting_charts(frame, rows[4]);
        if rows[5].height > 0 {
            if self.collector.operator_history.has_latency() && area.width >= 120 {
                let columns =
                    Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)])
                        .split(rows[5]);
                self.draw_signal_log(frame, columns[0]);
                self.draw_operator_chart(frame, columns[1], Chart::Latency);
            } else {
                self.draw_signal_log(frame, rows[5]);
            }
        }
    }

    fn draw_resource_charts(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        self.render_indicator_chart(
            frame,
            columns[0],
            "memory",
            &self.collector.load_history,
            ChartMetric::Memory,
        );
        self.render_indicator_chart(
            frame,
            columns[1],
            "paging / I/O",
            &self.collector.swap_history,
            ChartMetric::Swap,
        );
    }

    fn draw_supporting_charts(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::horizontal([
            Constraint::Percentage(50),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);
        self.draw_request_chart(frame, columns[0]);
        self.render_indicator_chart(
            frame,
            columns[1],
            "cache",
            &self.collector.cache_history,
            ChartMetric::Cache,
        );
        self.draw_operator_chart(frame, columns[2], Chart::Queue);
    }

    fn draw_workload_charts(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::horizontal([
            Constraint::Percentage(40),
            Constraint::Percentage(35),
            Constraint::Percentage(25),
        ])
        .split(area);
        self.render_indicator_chart(
            frame,
            columns[0],
            "generation",
            &self.collector.generation_history,
            ChartMetric::Generation,
        );
        self.render_indicator_chart(
            frame,
            columns[1],
            "prefill",
            &self.collector.prefill_history,
            ChartMetric::Prefill,
        );
        self.render_indicator_chart(
            frame,
            columns[2],
            self.gpu_chart_title(),
            &self.collector.gpu_history,
            ChartMetric::Gpu,
        );
    }

    fn gpu_chart_title(&self) -> &'static str {
        if self.collector.current.has_nvidia_gpus() && self.collector.current.gpus.len() > 1 {
            "GPU max"
        } else {
            "GPU"
        }
    }

    fn render_indicator_chart(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &'static str,
        history: &VecDeque<ChartPoint>,
        metric: ChartMetric,
    ) {
        self.charts.register(Chart::from(metric), area);
        let zoom = usize::from(self.charts.zoom(Chart::from(metric)));
        let current = history.back().and_then(|point| point.value);
        let current_label = match metric {
            ChartMetric::Generation | ChartMetric::Prefill => current
                .map(|value| {
                    if area.width < 38 {
                        format!("{:.1}", value as f64 / 10.0)
                    } else {
                        format!("LIVE {:.1} tok/s", value as f64 / 10.0)
                    }
                })
                .unwrap_or_else(|| {
                    let label = chart_inactive_rate_label(metric, &self.collector.current);
                    if area.width < 38 {
                        label.replace(" active", "")
                    } else {
                        label
                    }
                }),
            ChartMetric::Cache => current
                .map(|value| {
                    if area.width < 28 {
                        format!("int {value}%")
                    } else {
                        format!("interval {value}%")
                    }
                })
                .unwrap_or_else(|| {
                    if area.width < 28 {
                        "int —".into()
                    } else {
                        "interval —".into()
                    }
                }),
            ChartMetric::Gpu => current
                .map(|value| format!("{value}%"))
                .unwrap_or_else(|| "—".into()),
            ChartMetric::Memory => current
                .map(|value| {
                    if area.width >= 24 {
                        format!("{value}% resident")
                    } else {
                        format!("{value}%")
                    }
                })
                .unwrap_or_else(|| "—".into()),
            ChartMetric::Swap => current.map(rate).unwrap_or_else(|| "—".into()),
        };
        let chart_tone = metric.chart_tone();
        let current_tone = history
            .back()
            .map(|point| point.tone)
            .unwrap_or(Tone::Muted);
        let current_tone = if metric == ChartMetric::Memory && current.is_some() {
            Tone::Cyan
        } else {
            current_tone
        };
        let rate_chart = matches!(metric, ChartMetric::Generation | ChartMetric::Prefill);
        let label_width = if metric == ChartMetric::Swap {
            12
        } else if rate_chart {
            6
        } else {
            5
        }
        .min(area.width.saturating_sub(2) as usize);
        let plot_width = (area.width.saturating_sub(2) as usize).saturating_sub(label_width);
        let visible_count = self.charts.visible_samples(Chart::from(metric), plot_width);
        let visible_missing = history
            .iter()
            .rev()
            .take(visible_count)
            .all(|point| point.value.is_none());
        let scale = chart_scale(history, metric, visible_count);
        let top_axis = chart_axis_label(metric, scale.1);
        let mut axis_label = if rate_chart || metric == ChartMetric::Swap {
            format!("{}–{} auto", chart_axis_label(metric, scale.0), top_axis)
        } else {
            "0–100%".into()
        };
        let (average, peak) = chart_stats_for_width(history, metric, visible_count);
        if metric == ChartMetric::Swap && peak == Some(0) {
            axis_label = "zero traffic".into();
        }
        let window = format!(
            "{} · {zoom}×",
            chart_window_label(history.len().min(visible_count), self.interval)
        );
        let name = if metric == ChartMetric::Memory && area.width < 30 {
            "RAM"
        } else if metric == ChartMetric::Swap && area.width < 34 {
            "paging"
        } else {
            name
        };
        let mut title = Line::from(vec![Span::styled(
            format!(" {name} "),
            Style::default()
                .fg(chart_tone.color())
                .add_modifier(Modifier::BOLD),
        )]);
        let reading = Line::from(Span::styled(
            format!(" {current_label} "),
            Style::default()
                .fg(current_tone.color())
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Right);
        // Whole metadata fields fit or disappear. Never clip a token rate,
        // unit, time window or PID to squeeze in lower-priority statistics.
        let mut details = vec![window.clone()];
        if average.is_some() {
            details.extend([
                format!("window avg {}", chart_stat_label(metric, average)),
                format!("peak {}", chart_stat_label(metric, peak)),
                axis_label,
                "older → now".into(),
            ]);
        }
        if !self.charts.expanded {
            let span = Span::styled(
                format!("· {} ", chart_window_label(visible_count, self.interval)),
                Style::default().fg(MUTED),
            );
            if title.width() + span.width() + reading.width() + 2
                <= usize::from(area.width.saturating_sub(2))
            {
                title.spans.push(span);
            }
        }
        if self.charts.expanded && area.width >= 38 {
            for detail in details {
                let span = Span::styled(format!(" · {detail}"), Style::default().fg(MUTED));
                if title.width() + span.width() + reading.width() + 2
                    <= usize::from(area.width.saturating_sub(2))
                {
                    title.spans.push(span);
                }
            }
        }
        let mut block = Block::default()
            .title(title)
            .title(reading)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(PANEL));
        let sample = &self.collector.current;
        let summary = match metric {
            ChartMetric::Generation
                if self.charts.expanded
                    && sample.llm_generation_tps.is_some()
                    && !sample.llm_generation_tps_live =>
            {
                Some(llm_generation_rate_label(sample))
            }
            ChartMetric::Prefill
                if self.charts.expanded
                    && sample.llm_prefill_tps.is_some()
                    && !sample.llm_prefill_tps_live =>
            {
                Some(llm_prefill_rate_label(sample))
            }
            ChartMetric::Generation | ChartMetric::Prefill => Some("tok/s · auto".into()),
            ChartMetric::Memory => Some(if area.width < 24 {
                pressure_state_label(sample).into()
            } else {
                format!("PRESSURE {}", pressure_state_label(sample))
            }),
            ChartMetric::Gpu => Some(if current == Some(0) {
                "idle".into()
            } else {
                gpu_load_label(sample.gpu_util, self.thresholds).into()
            }),
            ChartMetric::Swap => Some(format!(
                "IN {} OUT {}",
                if sample.rate_ready {
                    rate(sample.swap_in).replace(' ', "")
                } else {
                    "—".into()
                },
                if sample.rate_ready {
                    rate(sample.swap_out).replace(' ', "")
                } else {
                    "—".into()
                }
            )),
            ChartMetric::Cache => Some(if area.width < 38 {
                format!("TOTAL {}", percent(sample.llm_cache_efficiency))
            } else {
                format!(
                    "TOTAL {} · PREFIX HIT {}",
                    percent(sample.llm_cache_efficiency),
                    percent(sample.llm_prefix_hit_rate)
                )
            }),
        };
        if let Some(summary) = summary {
            let summary = if rate_chart {
                if area.width < 38 {
                    summary.replace(" GEN", "").replace(" PREFILL", "")
                } else {
                    summary.replacen("AVG ", "SERVER AVG ", 1)
                }
            } else if metric == ChartMetric::Memory
                && Line::from(summary.as_str()).width() + 2
                    > usize::from(area.width.saturating_sub(2))
            {
                summary.replace("process ", "OS ")
            } else {
                summary
            };
            block = block.title_bottom(Line::from(Span::styled(
                format!(" {summary} "),
                if metric == ChartMetric::Memory {
                    Style::default()
                        .fg(sample.pressure_tone.color())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(chart_tone.color())
                },
            )));
        }
        let mut inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }
        if metric == ChartMetric::Memory && inner.height >= 5 {
            let usage = sample
                .resident_memory
                .map(|used| format!("{} / {}", bytes(used), bytes(sample.total_memory)))
                .unwrap_or_else(|| "RAM reading unavailable".into());
            let combined = format!("{usage} · Includes file cache");
            let lines = if Line::from(combined.as_str()).width() <= usize::from(inner.width) {
                vec![combined]
            } else {
                vec![usage, "Includes file cache".into()]
            };
            for text in lines {
                frame.render_widget(
                    Paragraph::new(text).style(Style::default().fg(MUTED)),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                inner.y += 1;
                inner.height -= 1;
            }
        }
        if metric == ChartMetric::Swap {
            swap_usage::draw(
                frame,
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                sample,
            );
            inner.height = inner.height.saturating_sub(1);
            if inner.is_empty() {
                return;
            }
            if inner.height < 3 {
                frame.render_widget(
                    Paragraph::new(match current {
                        Some(0) => "No paging traffic",
                        Some(_) => "Paging active",
                        None => "Paging unavailable",
                    })
                    .style(Style::default().fg(current_tone.color())),
                    inner,
                );
                return;
            }
        }
        if metric == ChartMetric::Cache && area.width < 38 && inner.height >= 4 {
            frame.render_widget(
                Paragraph::new(format!(
                    "PREFIX HIT {}",
                    percent(sample.llm_prefix_hit_rate)
                ))
                .style(Style::default().fg(MUTED)),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            inner.y += 1;
            inner.height -= 1;
        }
        let compact_percent = inner.height < 3
            && matches!(
                metric,
                ChartMetric::Memory | ChartMetric::Gpu | ChartMetric::Cache
            );
        if compact_percent {
            let measured = current.map(|value| (value as f64, format!("{value}%"), current_tone));
            if let Some((value, label, tone)) = measured {
                frame.render_widget(
                    Gauge::default()
                        .ratio((value / 100.0).clamp(0.0, 1.0))
                        .label(label)
                        .use_unicode(true)
                        .gauge_style(Style::default().fg(tone.color()).bg(EDGE)),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                if inner.height > 1 {
                    frame.render_widget(
                        Paragraph::new("Enter: history").style(Style::default().fg(MUTED)),
                        Rect::new(inner.x, inner.y + 1, inner.width, 1),
                    );
                }
            } else {
                frame.render_widget(
                    Paragraph::new(if inner.width < 19 {
                        "Unavailable"
                    } else {
                        "Reading unavailable"
                    })
                    .style(Style::default().fg(MUTED)),
                    inner,
                );
            }
            return;
        }
        if visible_missing {
            let message = match metric {
                _ if history.iter().any(|point| point.value.is_some()) => "No samples in view",
                ChartMetric::Cache if inner.width < 22 => "No cache samples",
                ChartMetric::Cache => "No interval cache data",
                ChartMetric::Generation | ChartMetric::Prefill
                    if sample.llm_status == "idle" && inner.width >= 28 =>
                {
                    "Idle · no live rate samples"
                }
                ChartMetric::Generation | ChartMetric::Prefill => "No live rate samples",
                _ => "No samples available",
            };
            frame.render_widget(
                Paragraph::new(message)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(MUTED)),
                Rect::new(
                    inner.x,
                    inner.y + inner.height.saturating_sub(1) / 2,
                    inner.width,
                    1,
                ),
            );
            return;
        }
        // Zero is a measurement, not an absent sample. Keep the zero trace
        // and its gaps, but do not invent a 1 B/s ceiling for an idle window.
        let paging_idle = metric == ChartMetric::Swap && peak == Some(0);
        if rate_chart && inner.height < 3 {
            frame.render_widget(
                Paragraph::new("Enter: history").style(Style::default().fg(MUTED)),
                inner,
            );
            return;
        }

        // Historical observations end where sampling stopped. Explain the
        // trailing gap without drawing a zero or holding the old rate live.
        if current.is_none() && inner.height >= 4 {
            if let Some((observed_at, value)) = history
                .iter()
                .rev()
                .take(visible_count)
                .find_map(|point| point.value.map(|value| (point.observed_at, value)))
            {
                let value_label = chart_stat_label(metric, Some(value));
                let mut label = if rate_chart {
                    format!("LAST SAMPLE {value_label} tok/s")
                } else if metric == ChartMetric::Cache {
                    format!("LAST INTERVAL {value_label}")
                } else {
                    format!("LAST {value_label}")
                };
                if Line::from(label.as_str()).width() > usize::from(inner.width) {
                    label = if rate_chart {
                        format!("LAST {value_label} tok/s")
                    } else {
                        format!("LAST INT {value_label}")
                    };
                }
                let with_age = format!("{label} · {}", telemetry_age(Some(observed_at)));
                if Line::from(with_age.as_str()).width() <= usize::from(inner.width) {
                    label = with_age;
                }
                frame.render_widget(
                    Paragraph::new(label).style(Style::default().fg(MUTED)),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                inner.y += 1;
                inner.height -= 1;
            }
        }

        let plot_height = inner.height as usize;
        // Render the newest sample at the right edge and let older samples
        // leave from the left. Every displayed column maps to one captured
        // sample: smoothing uses only the causal prefix and the current chart
        // resolution. Auto-scaling changes coordinates, never captured values,
        // ordering or colors; there is no future-sample smoothing.
        let points = chart_columns_for_plot(history, visible_count, metric, plot_height, scale);
        let mut visible_points = stretch_chart_columns(&points, plot_width);
        if metric == ChartMetric::Memory {
            for point in &mut visible_points {
                if point.value.is_some() {
                    point.tone = Tone::Cyan;
                }
            }
        }
        let mut cells = vec![
            vec![
                TraceCell {
                    glyph: ' ',
                    tone: Tone::Muted,
                };
                plot_width
            ];
            plot_height
        ];
        for (row, row_cells) in cells.iter_mut().enumerate() {
            let guide = if row + 1 == plot_height {
                Some('─')
            } else if row == plot_height.saturating_sub(1) / 2 && !paging_idle {
                Some('┄')
            } else {
                None
            };
            if let Some(glyph) = guide {
                row_cells.fill(TraceCell {
                    glyph,
                    tone: Tone::Muted,
                });
            }
        }

        let mut point_rows = vec![None; plot_width];
        let mut connect_before = vec![false; plot_width];
        let mut previous_point = None;
        for (column, point) in visible_points.iter().enumerate() {
            let Some(value) = point.value else {
                previous_point = None;
                continue;
            };
            if point.break_before {
                previous_point = None;
            }
            let display_value = chart_display_value(metric, value, scale);
            let Some((row, glyph)) = trace_point(display_value, plot_height) else {
                previous_point = None;
                continue;
            };
            if previous_point.is_some() {
                connect_before[column] = true;
            }
            point_rows[column] = Some((row, point.tone));
            cells[row][column] = TraceCell {
                glyph: if previous_point.is_none()
                    && visible_points
                        .get(column + 1)
                        .is_none_or(|next| next.value.is_none() || next.break_before)
                {
                    '●'
                } else {
                    glyph
                },
                tone: point.tone,
            };
            previous_point = Some((row, point.tone));
        }

        for column in 1..plot_width {
            if !connect_before[column] {
                continue;
            }
            let (Some((previous_row, previous_tone)), Some((row, tone))) =
                (point_rows[column - 1], point_rows[column])
            else {
                continue;
            };
            trace_connector(
                &mut cells,
                column,
                previous_row,
                row,
                TraceStyle {
                    metric,
                    previous_tone,
                    tone,
                    thresholds: self.thresholds,
                    scale,
                },
            );
        }

        let mut lines = Vec::with_capacity(plot_height);
        for (row, row_cells) in cells.iter().enumerate() {
            let label = if row == 0 && !paging_idle {
                format!("{top_axis} ")
            } else if row + 1 == plot_height {
                format!("{} ", chart_axis_label(metric, scale.0))
            } else if row == plot_height.saturating_sub(1) / 2
                && !paging_idle
                && scale.1.saturating_sub(scale.0) > 1
            {
                format!(
                    "{} ",
                    chart_axis_label(metric, scale.0 + (scale.1 - scale.0) / 2)
                )
            } else {
                String::new()
            };
            let mut spans = vec![Span::styled(
                format!("{label:>label_width$}"),
                Style::default().fg(DIM),
            )];
            let mut run = String::new();
            let mut run_tone = None;
            for cell in row_cells.iter().take(plot_width).copied() {
                let (cell, tone) = (cell.glyph, cell.tone);
                if run_tone != Some(tone) {
                    if let Some(tone) = run_tone {
                        spans.push(Span::styled(
                            std::mem::take(&mut run),
                            Style::default().fg(tone.color()),
                        ));
                    }
                    run_tone = Some(tone);
                }
                run.push(cell);
            }
            if let Some(tone) = run_tone {
                spans.push(Span::styled(run, Style::default().fg(tone.color())));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), inner);
        if paging_idle && inner.height >= 4 {
            frame.render_widget(
                Paragraph::new(if inner.width >= 32 {
                    "No paging traffic in this window"
                } else {
                    "No paging traffic"
                })
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED)),
                Rect::new(inner.x, inner.y + inner.height / 2 - 1, inner.width, 1),
            );
        }
    }

    fn draw_signal_log(&self, frame: &mut Frame, area: Rect) {
        let block = panel("RECENT JOURNAL", Tone::Muted)
            .title(
                Line::from(Span::styled(" 3 open ", Style::default().fg(CYAN)))
                    .alignment(Alignment::Right),
            )
            .title_bottom(Line::from(Span::styled(
                format!(" latest first · {} events ", self.collector.signals.len()),
                Style::default().fg(MUTED),
            )));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }
        if self.collector.signals.is_empty() {
            frame.render_widget(
                Paragraph::new("No events yet. Requests and resource changes appear here.")
                    .style(Style::default().fg(MUTED))
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        let message_x = inner.x + 24.min(inner.width);
        let message_width = inner.right().saturating_sub(message_x);
        let mut y = inner.y;
        for event in self.collector.signals.iter().rev() {
            if y >= inner.bottom() || message_width == 0 {
                break;
            }
            let mut lines = Vec::new();
            let mut line = String::new();
            for word in event.summary.split_whitespace() {
                let next = if line.is_empty() {
                    word.to_owned()
                } else {
                    format!("{line} {word}")
                };
                if Line::from(next.as_str()).width() > usize::from(message_width)
                    && !line.is_empty()
                {
                    lines.push(std::mem::take(&mut line));
                    line = compact_label(word, usize::from(message_width));
                } else {
                    line = compact_label(&next, usize::from(message_width));
                }
            }
            if !line.is_empty() {
                lines.push(line);
            }
            if lines.is_empty() {
                lines.push(String::new());
            }
            let height = lines.len().min(2).min(usize::from(inner.bottom() - y));
            if lines.len() > height {
                lines[height - 1] = format!(
                    "{}…",
                    compact_label(
                        &lines[height - 1],
                        usize::from(message_width).saturating_sub(1)
                    )
                );
            }
            frame.render_widget(
                Paragraph::new(event.time.as_str()).style(Style::default().fg(MUTED)),
                Rect::new(inner.x, y, 9, 1),
            );
            frame.render_widget(
                Paragraph::new(compact_label(&event.state, 13)).style(
                    Style::default()
                        .fg(event.tone.color())
                        .add_modifier(Modifier::BOLD),
                ),
                Rect::new(inner.x + 10, y, 13, 1),
            );
            frame.render_widget(
                Paragraph::new(
                    lines
                        .into_iter()
                        .take(height)
                        .map(Line::from)
                        .collect::<Vec<_>>(),
                )
                .style(Style::default().fg(Color::White)),
                Rect::new(message_x, y, message_width, height as u16),
            );
            y += height as u16;
        }
    }

    fn draw_journal(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(10)])
            .split(area);
        self.draw_journal_header(frame, rows[0]);
        self.draw_journal_events(frame, rows[1]);
    }

    fn filtered_llm_processes(&self) -> Vec<LlmProcess> {
        let query = self.top_filter.to_ascii_lowercase();
        let mut rows = self
            .collector
            .current
            .llm_processes
            .iter()
            .filter(|process| {
                query.is_empty()
                    || process.name.to_ascii_lowercase().contains(&query)
                    || process.command.to_ascii_lowercase().contains(&query)
            })
            .cloned()
            .collect::<Vec<_>>();
        match self.top_sort {
            TopSort::Rss => rows.sort_by(|left, right| {
                right
                    .rss
                    .cmp(&left.rss)
                    .then_with(|| left.pid.cmp(&right.pid))
            }),
            TopSort::Cpu => rows.sort_by(|left, right| {
                right
                    .cpu
                    .partial_cmp(&left.cpu)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| left.pid.cmp(&right.pid))
            }),
            TopSort::Pid => rows.sort_by_key(|process| process.pid),
            TopSort::Name => rows.sort_by(|left, right| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
                    .then_with(|| left.pid.cmp(&right.pid))
            }),
        }
        rows
    }

    fn draw_llm_top(&self, frame: &mut Frame, area: Rect) {
        let details = self.collector.current.llm_details.as_deref();
        let rows = Layout::vertical([
            Constraint::Length(if details.is_some() { 5 } else { 4 }),
            Constraint::Min(6),
            Constraint::Length(7),
        ])
        .split(area);
        let sample = &self.collector.current;
        let filtered = self.filtered_llm_processes();
        let total_rss = filtered.iter().map(|p| p.rss).sum::<u64>();
        let total_cpu = filtered.iter().map(|p| p.cpu).sum::<f64>();
        let filter_label = if self.top_filter.is_empty() {
            "all local LLM processes".to_owned()
        } else {
            format!("filter /{}", self.top_filter)
        };
        frame.render_widget(
            Paragraph::new(
                [
                    Line::from(Span::styled(
                        format!(
                            " {} of {} processes · RSS {} · CPU {:.1}% · sort {}",
                            filtered.len(),
                            sample.llm_processes.len(),
                            bytes(total_rss),
                            total_cpu,
                            self.top_sort.label()
                        ),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        format!(
                            " {}{}",
                            filter_label,
                            if self.top_filtering {
                                " · typing · Enter/Esc finish"
                            } else {
                                " · ↑↓ select · s sort · / filter · c clear"
                            }
                        ),
                        Style::default().fg(if self.top_filtering { YELLOW } else { MUTED }),
                    )),
                ]
                .into_iter()
                .chain(details.map(|text| {
                    Line::from(Span::styled(
                        format!(" API · {text}"),
                        Style::default().fg(CYAN),
                    ))
                }))
                .collect::<Vec<_>>(),
            )
            .block(panel("PROCESS MONITOR · LOCAL OS READINGS", Tone::Cyan)),
            rows[0],
        );
        let selected = self.top_selected.min(filtered.len().saturating_sub(1));
        let visible = rows[1].height.saturating_sub(3).max(1) as usize;
        let start = selected
            .saturating_sub(visible.saturating_sub(1))
            .min(filtered.len().saturating_sub(visible));
        let table_mode = if rows[1].width >= 150 {
            2
        } else if rows[1].width >= 95 {
            1
        } else {
            0
        };
        let (headers, widths): (Vec<&str>, Vec<Constraint>) = match table_mode {
            2 => (
                vec![
                    "PID", "PROCESS", "CPU", "MEM%", "RSS", "PAGEIN/s", "OS STATE", "PROVIDER",
                    "COMMAND",
                ],
                vec![
                    Constraint::Length(8),
                    Constraint::Length(20),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Length(13),
                    Constraint::Length(11),
                    Constraint::Length(9),
                    Constraint::Length(12),
                    Constraint::Min(20),
                ],
            ),
            1 => (
                vec![
                    "PID", "PROCESS", "CPU", "RSS", "PAGEIN/s", "OS STATE", "PROVIDER",
                ],
                vec![
                    Constraint::Length(7),
                    Constraint::Min(20),
                    Constraint::Length(8),
                    Constraint::Length(12),
                    Constraint::Length(10),
                    Constraint::Length(9),
                    Constraint::Length(12),
                ],
            ),
            _ => (
                vec!["PID", "PROCESS", "CPU", "RSS", "OS STATE"],
                vec![
                    Constraint::Length(7),
                    Constraint::Min(22),
                    Constraint::Length(8),
                    Constraint::Length(12),
                    Constraint::Length(9),
                ],
            ),
        };
        let table_rows = filtered
            .iter()
            .skip(start)
            .take(visible)
            .map(|process| {
                let pagein = process
                    .pagein_rate
                    .map(|value| format!("{value:.1}"))
                    .unwrap_or_else(|| "—".into());
                let state = if process.state == "?" {
                    "—"
                } else {
                    &process.state
                };
                let provider =
                    process_provider(&process.name, &process.command).unwrap_or_else(|| "—".into());
                let mut cells = vec![
                    Cell::from(process.pid.to_string()),
                    Cell::from(process.name.clone()),
                    Cell::from(format!("{:.1}%", process.cpu)),
                ];
                if table_mode == 2 {
                    cells.push(Cell::from(
                        process
                            .memory_percent
                            .map(|value| format!("{value:.1}%"))
                            .unwrap_or_else(|| "—".into()),
                    ));
                }
                cells.push(Cell::from(bytes(process.rss)));
                if table_mode > 0 {
                    cells.push(Cell::from(pagein));
                }
                cells.push(Cell::from(state.to_owned()));
                if table_mode > 0 {
                    cells.push(Cell::from(provider));
                }
                if table_mode == 2 {
                    cells.push(Cell::from(process.command.clone()));
                }
                Row::new(cells)
            })
            .collect::<Vec<_>>();
        let title = format!(
            "LLM PROCESSES · {}–{} of {}",
            if filtered.is_empty() { 0 } else { start + 1 },
            (start + visible).min(filtered.len()),
            filtered.len()
        );
        let table = Table::new(table_rows, widths)
            .header(
                Row::new(headers).style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
            )
            .row_highlight_style(
                Style::default()
                    .bg(Color::Rgb(35, 48, 67))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸ ")
            .block(panel(&title, Tone::Blue));
        let mut table_state = TableState::default();
        if !filtered.is_empty() {
            table_state.select(Some(selected.saturating_sub(start)));
        }
        frame.render_stateful_widget(table, rows[1], &mut table_state);

        let Some(process) = filtered.get(selected) else {
            let message = if self.top_filter.is_empty() {
                "No local LLM processes detected. Provider telemetry can still be available in Overview."
            } else {
                "No processes match this filter. Press c to clear it or / to edit."
            };
            frame.render_widget(
                Paragraph::new(message)
                    .wrap(Wrap { trim: true })
                    .block(panel("SELECTED PROCESS", Tone::Muted)),
                rows[2],
            );
            return;
        };
        let provider = process_provider(&process.name, &process.command);
        let mut detail = vec![Line::from(format!(
            "CPU {:.1}% · RSS {} · RAM {} · OS {} · PAGEIN {} pages/s",
            process.cpu,
            bytes(process.rss),
            process
                .memory_percent
                .map(|v| format!("{v:.1}%"))
                .unwrap_or_else(|| "—".into()),
            process.state,
            process
                .pagein_rate
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "—".into())
        ))];
        if sample
            .process_memory
            .as_ref()
            .is_some_and(|reading| reading.pid == process.pid)
        {
            detail.push(Line::from(format!(
                "OS footprint {} · {}",
                bytes(sample.process_memory.as_ref().unwrap().footprint),
                process_memory::detail(sample)
            )));
        }
        // Provider telemetry is runtime-wide; never attribute it to every PID.
        if provider.as_deref() == Some(sample.llm_provider.as_str()) {
            detail.push(Line::from(Span::styled(
                format!(
                    "RUNTIME {} · {} · {} · {}",
                    sample.llm_provider,
                    sample.llm_model,
                    sample.llm_status.to_uppercase(),
                    telemetry_source(sample)
                ),
                Style::default().fg(CYAN),
            )));
        } else {
            detail.push(Line::from(Span::styled(
                format!(
                    "RUNTIME {} · model/state unavailable",
                    provider.as_deref().unwrap_or("unknown")
                ),
                Style::default().fg(MUTED),
            )));
        }
        detail.push(Line::from(format!("COMMAND {}", process.command)));
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: true })
                .block(panel(
                    &format!("SELECTED PROCESS · PID {} · {}", process.pid, process.name),
                    Tone::Cyan,
                )),
            rows[2],
        );
    }

    fn filtered_journal_events(&self) -> Vec<&SignalEvent> {
        self.collector
            .signals
            .iter()
            .filter(|event| self.journal_filter.matches(event.kind))
            .collect()
    }

    fn draw_journal_header(&self, frame: &mut Frame, area: Rect) {
        let filtered_events = self.filtered_journal_events();
        let event_count = filtered_events.len();
        let total_count = self.collector.signals.len();
        let latest = filtered_events
            .last()
            .map(|event| event.summary.as_str())
            .unwrap_or("waiting for the first recorded event");
        let latest_tone = filtered_events
            .last()
            .map(|event| event.tone)
            .unwrap_or(Tone::Muted);
        let latest_age = filtered_events
            .last()
            .map(|event| telemetry_age(Some(event.recorded_at)))
            .unwrap_or_else(|| "age —".into());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        " JOURNAL  ",
                        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "{event_count} {} events · {total_count} total · f/[/] filter",
                            self.journal_filter.label(),
                        ),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled("  ·  ", Style::default().fg(DIM)),
                    Span::styled("meaningful changes only", Style::default().fg(MUTED)),
                ]),
                Line::from(vec![
                    Span::styled("  LATEST  ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("{latest} · {latest_age}"),
                        Style::default().fg(latest_tone.color()),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  SCOPE   ", Style::default().fg(MUTED)),
                    Span::styled(
                        "what changed · why it matters · what recovered",
                        Style::default().fg(CYAN),
                    ),
                    Span::styled("  ·  ↑ newer · ↓ older", Style::default().fg(DIM)),
                ]),
            ])
            .block(panel("EVENT JOURNAL · IMPACT TIMELINE", Tone::Cyan))
            .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn draw_journal_events(&self, frame: &mut Frame, area: Rect) {
        let capacity = area.height.saturating_sub(2) as usize;
        let filtered_events = self.filtered_journal_events();
        let max_scroll = filtered_events.len().saturating_sub(capacity);
        let scroll = self.journal_scroll.min(max_scroll);
        let lines = filtered_events
            .iter()
            .rev()
            .skip(scroll)
            .take(capacity)
            .map(|event| {
                Line::from(vec![
                    Span::styled(format!(" {} ", event.time), Style::default().fg(DIM)),
                    Span::styled(
                        format!("{:^16}", compact_label(&event.state, 16)),
                        Style::default()
                            .fg(event.tone.color())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  │  ", Style::default().fg(DIM)),
                    Span::styled(&event.summary, Style::default().fg(Color::White)),
                ])
            })
            .collect::<Vec<_>>();
        let text = if lines.is_empty() {
            Text::from(Line::from(Span::styled(
                " waiting for the first recorded event",
                Style::default().fg(MUTED),
            )))
        } else {
            Text::from(lines)
        };
        let title = format!(
            "EVENTS · {} · {}–{} of {}",
            self.journal_filter.label(),
            if filtered_events.is_empty() {
                0
            } else {
                scroll + 1
            },
            (scroll + capacity).min(filtered_events.len()),
            filtered_events.len()
        );
        frame.render_widget(
            Paragraph::new(text)
                .block(panel(&title, Tone::Blue))
                .wrap(Wrap { trim: true }),
            area,
        );
        if filtered_events.len() > capacity {
            let mut scrollbar_state = ScrollbarState::new(filtered_events.len()).position(scroll);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(CYAN))
                    .track_style(Style::default().fg(DIM)),
                area,
                &mut scrollbar_state,
            );
        }
    }

    fn draw_controls(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Block::default().style(Style::default().bg(PANEL_RAISED)),
            area,
        );
        let help = Line::from(vec![
            Span::styled(
                " ?",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" help ", Style::default().fg(MUTED)),
            Span::styled(
                "q",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" quit ", Style::default().fg(MUTED)),
        ]);
        let available = usize::from(area.width).saturating_sub(help.width());
        let mut line = Line::from(Span::styled(
            if self.tab == 0 {
                format!(
                    " {} · {}× ",
                    self.charts.focused.label(),
                    self.charts.zoom(self.charts.focused)
                )
            } else if self.tab == 1 {
                " MLX Top ".into()
            } else {
                " Journal ".into()
            },
            Style::default().fg(CYAN),
        ));
        let hints = match self.tab {
            0 => vec![
                (
                    "Enter",
                    if self.charts.expanded {
                        "restore"
                    } else {
                        "expand"
                    },
                ),
                ("↑↓←→", "chart"),
                ("+/−", "zoom"),
                ("Tab", "view"),
                ("p", if self.paused { "resume" } else { "pause" }),
                ("Shift+↑↓", "requests"),
                ("{ / }", "interval"),
            ],
            1 => vec![
                ("↑↓", "select"),
                ("s", "sort"),
                ("/", "filter"),
                ("Tab", "view"),
                ("p", "pause"),
            ],
            _ => vec![
                ("↑↓", "scroll"),
                ("f", "filter"),
                ("Tab", "view"),
                ("r", "reset"),
                ("p", "pause"),
            ],
        };
        for (key, label) in hints {
            let key = Span::styled(
                format!(" {key}"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            let label = Span::styled(format!(" {label} "), Style::default().fg(MUTED));
            if line.width() + key.width() + label.width() <= available {
                line.spans.extend([key, label]);
            }
        }
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y, available as u16, 1),
        );
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(
                area.x + available as u16,
                area.y,
                area.width - available as u16,
                1,
            ),
        );
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let popup = if area.width < 120 || area.height < 36 {
            centered_rect(94, 88, area)
        } else {
            centered_rect(60, 58, area)
        };
        frame.render_widget(Clear, popup);
        let mut text = vec![
            Line::from(Span::styled(
                "CONTROLS",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("1 / 2 / 3       Overview / MLX Top / Journal"),
            Line::from("Tab / Shift-Tab next / previous view"),
            Line::from("p / Space       pause or resume; r resets history"),
            Line::from("{ / }           change refresh interval (1–60s)"),
            Line::from("a               acknowledge critical system alarm"),
            Line::from("q / Ctrl-C      quit; ? / h closes help"),
            Line::from(""),
        ];
        text.extend(match self.tab {
            0 => vec![
                Line::from("Arrow keys      select a neighboring chart"),
                Line::from("+ / - / wheel   zoom (time series linked); 0 reset"),
                Line::from("Enter / Esc     enlarge / restore chart"),
                Line::from("Mouse click     select chart; right-click enlarge"),
                Line::from("Shift-↑↓        newer / older prompt"),
                Line::from("Home / End      newest / oldest prompt"),
                Line::from("PgUp / PgDn     move by ten requests"),
                Line::from(if self.collector.current.has_nvidia_gpus() {
                    "[ / ] GPUs      select previous / next NVIDIA card"
                } else {
                    ""
                }),
            ],
            1 => vec![
                Line::from("↑↓ / PgUp/PgDn  select process / move ten rows"),
                Line::from("Home / End      first / last process"),
                Line::from("s               cycle RSS / CPU / PID / name sort"),
                Line::from("f or /          filter processes; c clears filter"),
            ],
            _ => vec![
                Line::from("↑↓ / PgUp/PgDn  newer / older events"),
                Line::from("Home / End      newest / oldest event"),
                Line::from("f / [ / ]       cycle event filters"),
            ],
        });
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(
                Block::default()
                    .title(" HELP ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(CYAN))
                    .style(Style::default().bg(PANEL)),
            ),
            popup,
        );
    }

    fn draw_compact_warning(&self, frame: &mut Frame, area: Rect) {
        let text = vec![
            Line::from(Span::styled(
                "mlxtop",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("This dashboard needs at least 72×24 terminal cells."),
            Line::from(format!("Current size: {}×{}", area.width, area.height)),
            Line::from("Resize the terminal, or use --once for a static report."),
            Line::from("q quit"),
        ];
        frame.render_widget(
            Paragraph::new(text).alignment(Alignment::Center).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(YELLOW)),
            ),
            area,
        );
    }
}

fn threshold_with_hysteresis(value: u64, was_active: bool, enter: u64, exit: u64) -> bool {
    value >= if was_active { exit } else { enter }
}

fn classify(sample: &mut Sample, previous: Option<&Sample>, thresholds: Thresholds) {
    let swap_churn = sample.swap_in.saturating_add(sample.swap_out);
    let comp_churn = sample.compress.saturating_add(sample.decompress);
    let llm = llm_is_observed(sample);
    let previous_impact = previous.map(|sample| sample.impact.as_str());
    let was_paging = matches!(
        previous_impact,
        Some("PAGING ACTIVE" | "WATCH PAGING" | "HEAVY PAGING" | "SWAP THRASHING")
    );
    let was_compressing = matches!(previous_impact, Some("COMPRESSION ACTIVE"));
    let was_gpu_busy = matches!(previous_impact, Some("GPU BUSY"));
    let swap_critical_rate = thresholds.swap_critical_rate;
    let paging_active = threshold_with_hysteresis(
        swap_churn,
        was_paging,
        PAGING_ACTIVE_ENTER_RATE,
        thresholds.swap_warn_exit,
    );
    let compression_active = threshold_with_hysteresis(
        comp_churn,
        was_compressing,
        thresholds.compression_warn_rate,
        thresholds.compression_warn_exit,
    );
    let watch_paging = swap_churn >= thresholds.swap_warn_rate;
    let swap_thrashing =
        sample.swap_in >= swap_critical_rate && sample.swap_out >= swap_critical_rate;
    let heavy_paging = sample.swap_out >= 2 * swap_critical_rate
        || sample.swap_growth >= (2 * swap_critical_rate) as i64;
    let page_in_recovery = sample.swap_in >= 2 * swap_critical_rate && sample.swap_growth <= 0;
    let gpu_busy = threshold_with_hysteresis(
        sample.gpu_util.unwrap_or_default() as u64,
        was_gpu_busy,
        GPU_BUSY_ENTER_LOAD,
        thresholds.gpu_warn_exit,
    ) && llm;
    let native_counters_available = sample.total_memory > 0
        && sample.availability.is_some()
        && sample.pressure != "UNKNOWN"
        && sample.vm_available
        && sample.swap_available;
    let (impact, tone, health, grade, limiter) = if !sample.rate_ready {
        (
            "SAMPLING",
            Tone::Cyan,
            None,
            "SAMPLING",
            "collecting baseline",
        )
    } else if !native_counters_available {
        (
            "DATA LIMITED",
            Tone::Muted,
            None,
            "UNKNOWN",
            "native counter unavailable",
        )
    } else if sample.pressure == "RED" {
        (
            "MEMORY BOTTLENECK",
            Tone::Red,
            Some(10),
            "CRITICAL",
            "memory pressure",
        )
    } else if swap_thrashing {
        ("SWAP THRASHING", Tone::Red, Some(20), "POOR", "swap thrash")
    } else if heavy_paging {
        ("HEAVY PAGING", Tone::Red, Some(30), "POOR", "disk paging")
    } else if page_in_recovery {
        (
            "PAGE-IN RECOVERY",
            Tone::Red,
            Some(45),
            "RECOVERING",
            "page-in recovery",
        )
    } else if sample.pressure == "YELLOW" {
        (
            "MEMORY STRESS",
            Tone::Yellow,
            Some(55),
            "DEGRADED",
            "tight memory",
        )
    } else if sample.thermal.starts_with("limited") || sample.thermal == "warning reported" {
        (
            "THERMAL LIMIT",
            Tone::Yellow,
            Some(60),
            "DEGRADED",
            "thermal limit",
        )
    } else if paging_active {
        (
            "PAGING ACTIVE",
            Tone::Yellow,
            Some(65),
            "CONSTRAINED",
            "active paging",
        )
    } else if compression_active {
        (
            "COMPRESSION ACTIVE",
            Tone::Yellow,
            Some(70),
            "CONSTRAINED",
            "compression churn",
        )
    } else if watch_paging {
        (
            "WATCH PAGING",
            Tone::Yellow,
            Some(85),
            "GOOD",
            "light paging",
        )
    } else if gpu_busy {
        ("GPU BUSY", Tone::Cyan, Some(100), "HEALTHY", "GPU activity")
    } else if llm {
        ("LLM READY", Tone::Green, Some(100), "HEALTHY", "none")
    } else {
        ("IDLE", Tone::Green, Some(100), "HEALTHY", "none")
    };
    sample.impact = impact.into();
    sample.impact_tone = tone;
    sample.health = health;
    sample.grade = grade.into();
    sample.limiter = limiter.into();

    let largest = sample.largest_consumer.as_deref().unwrap_or("largest app");
    if !sample.rate_ready {
        sample.guidance_badge = "WAIT".into();
        sample.guidance_cause = "Collecting the live-rate baseline.".into();
        sample.guidance_action = "Wait one refresh before acting.".into();
    } else if !native_counters_available {
        sample.guidance_badge = "CHECK".into();
        sample.guidance_cause = "One or more native counters are unavailable.".into();
        sample.guidance_action =
            "Run on supported macOS hardware and verify system command access.".into();
    } else if sample.pressure == "RED" {
        sample.guidance_badge = "ACT NOW".into();
        sample.guidance_cause =
            "Critical pressure — the system cannot reclaim RAM fast enough.".into();
        sample.guidance_action = if llm {
            "Stop unused models/requests; reduce context or concurrency.".into()
        } else {
            "Pause or quit the largest app first; wait for critical pressure to clear.".into()
        };
    } else if swap_thrashing {
        sample.guidance_badge = "ACT NOW".into();
        sample.guidance_cause = format!(
            "Swap thrash — {} in + {} out.",
            rate(sample.swap_in),
            rate(sample.swap_out)
        );
        sample.guidance_action = "Reduce model/context/KV cache or parallel requests.".into();
    } else if sample.swap_out >= swap_critical_rate
        || sample.swap_growth >= swap_critical_rate as i64
    {
        sample.guidance_badge = "ACT NOW".into();
        sample.guidance_cause =
            format!("RAM overflow — evicting {} to disk.", rate(sample.swap_out));
        sample.guidance_action = if llm {
            "Stop unused models or reduce model/context/cache.".into()
        } else {
            format!("Pause or quit {largest}; wait for swap-out to approach zero.")
        };
    } else if paging_active {
        sample.guidance_badge = "WATCH".into();
        sample.guidance_cause = format!(
            "Paging active — in {} + out {}.",
            rate(sample.swap_in),
            rate(sample.swap_out)
        );
        sample.guidance_action =
            "Reduce model/context/cache if paging persists during inference.".into();
    } else if compression_active {
        sample.guidance_badge = "WATCH".into();
        sample.guidance_cause = format!(
            "Compression active — {} compressed + {} decompressed.",
            rate(sample.compress),
            rate(sample.decompress)
        );
        sample.guidance_action =
            "Watch for rising paging or pressure; compression alone is not a bottleneck.".into();
    } else if watch_paging {
        sample.guidance_badge = "WATCH".into();
        sample.guidance_cause = format!("Light paging — {} total.", rate(swap_churn));
        sample.guidance_action =
            "No immediate action; investigate if the rate persists or rises.".into();
    } else if sample.pressure == "YELLOW" {
        sample.guidance_badge = "REDUCE".into();
        sample.guidance_cause = "Resident memory is tight; paging may follow.".into();
        sample.guidance_action = if llm {
            "Reduce model/context/cache or concurrency before adding requests.".into()
        } else {
            format!("Close an unneeded large app (start with {largest}).")
        };
    } else if sample.thermal.starts_with("limited") || sample.thermal == "warning reported" {
        sample.guidance_badge = "COOL".into();
        sample.guidance_cause = "Thermal limiting — memory is not the bottleneck.".into();
        sample.guidance_action =
            "Reduce batch/concurrency or pause until the warning clears.".into();
    } else if gpu_busy {
        sample.guidance_badge = "GPU".into();
        sample.guidance_cause =
            "GPU busy; utilization alone does not establish a bottleneck.".into();
        sample.guidance_action =
            "Compare throughput at the same prompt size and concurrency.".into();
    } else {
        sample.guidance_badge = "OK".into();
        sample.guidance_cause =
            "No active memory, paging, compression, or thermal bottleneck.".into();
        sample.guidance_action =
            "Nothing to fix; used swap can remain high after pressure passes.".into();
    }
}

fn signal_summary(sample: &Sample) -> String {
    match sample.impact.as_str() {
        "SWAP THRASHING" => format!(
            "in {} / out {}",
            rate(sample.swap_in),
            rate(sample.swap_out)
        ),
        "HEAVY PAGING" => format!(
            "swap {} / growth {}",
            rate(sample.swap_in + sample.swap_out),
            signed_rate(sample.swap_growth)
        ),
        "MEMORY BOTTLENECK" => format!(
            "native pressure critical · LLM RSS {} · RAM resident {}",
            bytes(sample.llm_rss),
            sample
                .resident_memory
                .map(bytes)
                .unwrap_or_else(|| "—".into())
        ),
        "PAGE-IN RECOVERY" => format!(
            "page-in {} · growth {}",
            rate(sample.swap_in),
            signed_rate(sample.swap_growth)
        ),
        "GPU BUSY" if !sample.correlation.summary.is_empty() => sample.correlation.summary.clone(),
        "GPU BUSY" => format!(
            "GPU {}% busy · {}",
            sample.gpu_util.unwrap_or(0),
            llm_generation_rate_label(sample)
        ),
        "LLM READY" => format!(
            "{} process(es) · {} · {} · cache {}",
            sample.llm_count,
            llm_generation_rate_label(sample),
            llm_prefill_rate_label(sample),
            percent(sample.llm_cache_efficiency)
        ),
        _ => sample.limiter.clone(),
    }
}

fn llm_is_observed(sample: &Sample) -> bool {
    sample.llm_count > 0
        || sample.llm_source == TelemetrySource::Live
        || sample.llm_generation_tps.is_some()
        || sample.llm_model != "not detected"
}

fn correlation_evidence_label(sample: &Sample) -> String {
    match sample.correlation.cause {
        CorrelationCause::Paging if sample.swap_in.saturating_add(sample.swap_out) > 0 => {
            format!(
                "I/O {}",
                rate(sample.swap_in.saturating_add(sample.swap_out))
            )
        }
        CorrelationCause::Compression if sample.compress.saturating_add(sample.decompress) > 0 => {
            format!(
                "compress {}",
                rate(sample.compress.saturating_add(sample.decompress))
            )
        }
        CorrelationCause::MemoryPressure => {
            format!("pressure {}", pressure_state_label(sample))
        }
        CorrelationCause::Thermal => "thermal limit".into(),
        CorrelationCause::MetalMemory => fraction(sample.gpu_in_use, sample.gpu_alloc)
            .map(|ratio| format!("Metal mem {:.0}%", ratio * 100.0))
            .unwrap_or_else(|| "Metal mem".into()),
        CorrelationCause::GpuSaturation => [
            sample.gpu_util,
            sample.metal.renderer_util,
            sample.metal.tiler_util,
        ]
        .into_iter()
        .flatten()
        .max()
        .map(|value| format!("GPU {value}%"))
        .unwrap_or_else(|| "GPU".into()),
        CorrelationCause::Queueing => sample
            .llm_waiting_requests
            .map(|waiting| format!("queue {waiting} waiting"))
            .unwrap_or_else(|| "queueing".into()),
        CorrelationCause::ContextGrowth => llm_context_tokens(sample)
            .map(|tokens| format!("context {}", compact_tokens(tokens)))
            .unwrap_or_else(|| "context/KV".into()),
        CorrelationCause::ModelMemory => {
            fraction(sample.llm_model_memory, sample.llm_model_memory_max)
                .map(|ratio| format!("model mem {:.0}%", ratio * 100.0))
                .unwrap_or_else(|| "model memory".into())
        }
        CorrelationCause::Runtime => "workload/runtime".into(),
        CorrelationCause::None => String::new(),
        CorrelationCause::Paging | CorrelationCause::Compression => String::new(),
    }
}

fn normalize_chart_value(metric: ChartMetric, value: u64) -> u64 {
    match metric {
        ChartMetric::Generation | ChartMetric::Prefill | ChartMetric::Swap => value,
        ChartMetric::Cache | ChartMetric::Memory | ChartMetric::Gpu => value.min(100),
    }
}

fn chart_stats<'a, I>(points: I, metric: ChartMetric) -> (Option<u64>, Option<u64>)
where
    I: IntoIterator<Item = &'a ChartPoint>,
{
    let mut count = 0_u64;
    let mut total = 0_u64;
    let mut peak = 0_u64;
    for value in points.into_iter().filter_map(|point| point.value) {
        // Paging stats stay in bytes/s: a percent of the log scale would be
        // meaningless next to the byte-rate label shown for the live value.
        let value = if matches!(
            metric,
            ChartMetric::Generation | ChartMetric::Prefill | ChartMetric::Swap
        ) {
            value
        } else {
            normalize_chart_value(metric, value)
        };
        count += 1;
        total = total.saturating_add(value);
        peak = peak.max(value);
    }
    if count == 0 {
        (None, None)
    } else {
        (
            Some(total.checked_div(count).unwrap_or_default()),
            Some(peak),
        )
    }
}

fn chart_stats_for_width(
    history: &VecDeque<ChartPoint>,
    metric: ChartMetric,
    width: usize,
) -> (Option<u64>, Option<u64>) {
    let visible_start = history.len().saturating_sub(width);
    chart_stats(history.iter().skip(visible_start), metric)
}

fn chart_stat_label(metric: ChartMetric, value: Option<u64>) -> String {
    match metric {
        ChartMetric::Generation | ChartMetric::Prefill => value
            .map(|value| format!("{:.1}", value as f64 / 10.0))
            .unwrap_or_else(|| "—".into()),
        ChartMetric::Swap => value.map(rate).unwrap_or_else(|| "—".into()),
        ChartMetric::Cache | ChartMetric::Memory | ChartMetric::Gpu => value
            .map(|value| format!("{value}%"))
            .unwrap_or_else(|| "—".into()),
    }
}

fn chart_scale(history: &VecDeque<ChartPoint>, metric: ChartMetric, width: usize) -> (u64, u64) {
    if matches!(metric, ChartMetric::Generation | ChartMetric::Prefill) {
        chart_scale::range(
            history
                .iter()
                .skip(history.len().saturating_sub(width))
                .filter_map(|point| point.value),
            10,
        )
    } else if metric == ChartMetric::Swap {
        (
            0,
            chart_scale::ceiling(
                history
                    .iter()
                    .skip(history.len().saturating_sub(width))
                    .filter_map(|point| point.value)
                    .max()
                    .unwrap_or(0),
                1,
            ),
        )
    } else {
        (0, 100)
    }
}

fn chart_axis_label(metric: ChartMetric, value: u64) -> String {
    if matches!(metric, ChartMetric::Generation | ChartMetric::Prefill) {
        if value >= 100_000 {
            compact_tokens(value / 10)
        } else if value.is_multiple_of(10) {
            (value / 10).to_string()
        } else {
            format!("{:.1}", value as f64 / 10.0)
        }
    } else if metric == ChartMetric::Swap {
        rate(value)
    } else {
        format!("{value}%")
    }
}

fn chart_display_value(metric: ChartMetric, value: u64, scale: (u64, u64)) -> u64 {
    if matches!(
        metric,
        ChartMetric::Generation | ChartMetric::Prefill | ChartMetric::Swap
    ) {
        (u128::from(value.saturating_sub(scale.0)) * 100
            / u128::from(scale.1.saturating_sub(scale.0).max(1)))
        .min(100) as u64
    } else {
        normalize_chart_value(metric, value)
    }
}

fn chart_window_label(samples: usize, interval: Duration) -> String {
    let seconds = (samples as u64).saturating_mul(interval.as_secs().max(1));
    if seconds >= 60 {
        format!("window {}m", seconds / 60)
    } else {
        format!("window {seconds}s")
    }
}

fn chart_columns(history: &VecDeque<ChartPoint>, width: usize) -> Vec<RenderPoint> {
    if width == 0 {
        return Vec::new();
    }

    let visible_start = history.len().saturating_sub(width);
    let left_padding = width.saturating_sub(history.len() - visible_start);
    let mut columns = vec![
        RenderPoint {
            value: None,
            tone: Tone::Muted,
            break_before: true,
        };
        width
    ];

    for (offset, point) in history.iter().skip(visible_start).enumerate() {
        columns[left_padding + offset] = RenderPoint {
            value: point.value,
            tone: point.tone,
            break_before: point.value.is_none(),
        };
    }
    columns
}

// Spread an identical sample window over any plot width without averaging,
// skipping spikes, or connecting across absent observations.
fn stretch_chart_columns(points: &[RenderPoint], width: usize) -> Vec<RenderPoint> {
    if points.is_empty() {
        return Vec::new();
    }
    (0..width)
        .map(|column| points[column * points.len() / width])
        .collect()
}

fn chart_columns_for_plot(
    history: &VecDeque<ChartPoint>,
    width: usize,
    metric: ChartMetric,
    plot_height: usize,
    scale: (u64, u64),
) -> Vec<RenderPoint> {
    let mut columns = chart_columns(history, width);
    let visible_start = history.len().saturating_sub(width);
    let left_padding = width.saturating_sub(history.len() - visible_start);
    let plot_values = chart_plot_values_scaled(history, metric, plot_height, scale);
    for (offset, value) in plot_values.iter().skip(visible_start).enumerate() {
        columns[left_padding + offset].value = *value;
    }
    columns
}

fn chart_plot_values_scaled(
    history: &VecDeque<ChartPoint>,
    metric: ChartMetric,
    plot_height: usize,
    scale: (u64, u64),
) -> Vec<Option<u64>> {
    let deadband = chart_visual_deadband(plot_height);
    let mut anchor = None;
    let mut values = Vec::with_capacity(history.len());
    for point in history {
        let Some(value) = point.value else {
            anchor = None;
            values.push(None);
            continue;
        };
        let plotted = match anchor {
            Some(previous) if chart_display_delta(metric, previous, value, scale) <= deadband => {
                previous
            }
            _ => value,
        };
        anchor = Some(plotted);
        values.push(Some(plotted));
    }
    values
}

fn chart_visual_deadband(plot_height: usize) -> f64 {
    let drawable_rows = plot_height.saturating_sub(1);
    if drawable_rows == 0 {
        100.0
    } else {
        100.0 / drawable_rows as f64 * CHART_VISUAL_DEADBAND_FRACTION
    }
}

fn chart_display_delta(metric: ChartMetric, left: u64, right: u64, scale: (u64, u64)) -> f64 {
    let left = chart_display_value(metric, left, scale) as f64;
    let right = chart_display_value(metric, right, scale) as f64;
    (left - right).abs()
}

fn trace_point(value: u64, height: usize) -> Option<(usize, char)> {
    if height == 0 {
        return None;
    }
    let value = value.min(100);
    let row_count = height.saturating_sub(1) as u64;
    let row_from_bottom = value.saturating_mul(row_count).saturating_add(50) / 100;
    let row = height - 1 - row_from_bottom.min(row_count) as usize;
    Some((row, '━'))
}

/**
 * How one chart segment should be coloured: which metric it belongs to, the
 * tones at each end of the segment, and the thresholds that band them.
 */
#[derive(Clone, Copy)]
struct TraceStyle {
    metric: ChartMetric,
    previous_tone: Tone,
    tone: Tone,
    thresholds: Thresholds,
    scale: (u64, u64),
}

fn trace_connector(
    cells: &mut [Vec<TraceCell>],
    column: usize,
    previous_row: usize,
    row: usize,
    style: TraceStyle,
) {
    let TraceStyle {
        metric,
        previous_tone,
        tone,
        thresholds,
        scale,
    } = style;
    if column >= cells.first().map(Vec::len).unwrap_or(0)
        || previous_row == row
        || previous_row >= cells.len()
        || row >= cells.len()
    {
        return;
    }
    let upper = previous_row.min(row);
    let lower = previous_row.max(row);
    let height = cells.len();
    for (offset, row_cells) in cells.iter_mut().enumerate().take(lower).skip(upper + 1) {
        let fraction = chart_row_value(offset, height);
        let display_value = if metric == ChartMetric::Swap {
            scale.0.saturating_add(
                (u128::from(scale.1.saturating_sub(scale.0)) * u128::from(fraction) / 100) as u64,
            )
        } else {
            fraction
        };
        row_cells[column] = TraceCell {
            glyph: '┃',
            tone: chart_transition_tone(metric, display_value, previous_tone, tone, thresholds),
        };
    }
    cells[upper][column] = TraceCell {
        glyph: if row > previous_row { '┓' } else { '┏' },
        tone: if upper == previous_row {
            previous_tone
        } else {
            tone
        },
    };
    cells[lower][column] = TraceCell {
        glyph: if row > previous_row { '┗' } else { '┛' },
        tone: if lower == previous_row {
            previous_tone
        } else {
            tone
        },
    };
}

fn chart_row_value(row: usize, height: usize) -> u64 {
    let row_count = height.saturating_sub(1) as u64;
    if row_count == 0 {
        return 0;
    }
    (height.saturating_sub(1).saturating_sub(row) as u64)
        .saturating_mul(100)
        .checked_div(row_count)
        .unwrap_or(0)
}

fn chart_transition_tone(
    metric: ChartMetric,
    display_value: u64,
    previous_tone: Tone,
    tone: Tone,
    thresholds: Thresholds,
) -> Tone {
    match metric {
        ChartMetric::Generation | ChartMetric::Prefill => {
            if previous_tone == tone {
                tone
            } else {
                Tone::Muted
            }
        }
        // Pressure has no numeric relationship to the connector's RAM height.
        // Use the newly captured state; never synthesize a warning while
        // crossing an occupancy percentage between two normal observations.
        ChartMetric::Cache | ChartMetric::Memory => tone,
        ChartMetric::Gpu => metric.tone(display_value, thresholds),
        ChartMetric::Swap => metric.tone(display_value, thresholds),
    }
}

fn llm_model_label(sample: &Sample, max_chars: usize) -> String {
    compact_label(&sample.llm_model, max_chars)
}

fn compact_label(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.into();
    }
    let mut label: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    label.push('…');
    label
}

fn tokens_per_second(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1} tok/s"))
        .unwrap_or_else(|| "—".into())
}

/// Return a live request rate for a chart. Aggregate session rates and
/// completion-log values are deliberately excluded because they are not
/// measurements of the current sample.
fn chart_rate_value(sample: &Sample, metric: ChartMetric) -> Option<u64> {
    let (value, live) = match metric {
        ChartMetric::Generation => (sample.llm_generation_tps, sample.llm_generation_tps_live),
        ChartMetric::Prefill => (sample.llm_prefill_tps, sample.llm_prefill_tps_live),
        _ => return None,
    };
    if !live {
        return None;
    }
    value
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| (value * 10.0).round() as u64)
}

fn cache_interval_efficiency(
    previous: &mut Option<CacheCounters>,
    telemetry: Option<&LlmTelemetry>,
    stale: bool,
) -> Option<f64> {
    if stale {
        *previous = None;
        return None;
    }
    let Some(telemetry) = telemetry else {
        *previous = None;
        return None;
    };
    let Some(counters) = telemetry
        .total_prompt_tokens
        .zip(telemetry.total_cached_tokens)
        .map(|(prompt_tokens, cached_tokens)| CacheCounters {
            provider: telemetry.provider.clone(),
            prompt_tokens,
            cached_tokens,
        })
    else {
        *previous = None;
        return None;
    };
    let previous = previous.replace(counters.clone())?;
    if previous.provider != telemetry.provider {
        return None;
    }
    let prompt_delta = counters.prompt_tokens.checked_sub(previous.prompt_tokens)?;
    if prompt_delta == 0 {
        return None;
    }
    let cached_delta = counters.cached_tokens.checked_sub(previous.cached_tokens)?;
    if cached_delta > prompt_delta {
        return None;
    }
    Some(cached_delta as f64 / prompt_delta as f64 * 100.0)
}

fn llm_rate_label(sample: &Sample, metric: &str, value: Option<f64>, live: bool) -> String {
    let label = if live {
        metric.to_owned()
    } else if value.is_some() {
        match sample.llm_source {
            TelemetrySource::Live => format!("AVG {metric}"),
            TelemetrySource::Log | TelemetrySource::Report => format!("LAST {metric}"),
            TelemetrySource::None => metric.to_owned(),
        }
    } else {
        metric.to_owned()
    };
    format!("{label} {}", tokens_per_second(value))
}

fn llm_generation_rate_label(sample: &Sample) -> String {
    llm_rate_label(
        sample,
        "GEN",
        sample.llm_generation_tps,
        sample.llm_generation_tps_live,
    )
}

fn llm_prefill_rate_label(sample: &Sample) -> String {
    llm_rate_label(
        sample,
        "PREFILL",
        sample.llm_prefill_tps,
        sample.llm_prefill_tps_live,
    )
}

fn chart_inactive_rate_label(metric: ChartMetric, sample: &Sample) -> String {
    match (metric, sample.llm_status.as_str()) {
        (ChartMetric::Generation, "prefilling") => "prefill active".into(),
        (ChartMetric::Generation, "generating") => "active".into(),
        (ChartMetric::Prefill, "prefilling") => "active".into(),
        (ChartMetric::Prefill, "generating") => "decode active".into(),
        (_, "idle" | "last result") => "idle".into(),
        (_, "waiting") => "waiting".into(),
        (_, "offline") => "offline".into(),
        _ => "—".into(),
    }
}

fn percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "—".into())
}

fn percent_u8(value: Option<u8>) -> String {
    value
        .map(|value| format!("{value}%"))
        .unwrap_or_else(|| "—".into())
}

fn optional_tokens(value: Option<u64>) -> String {
    value.map(compact_tokens).unwrap_or_else(|| "—".into())
}

fn llm_context_label(sample: &Sample) -> String {
    llm_context_tokens(sample)
        .map(compact_tokens)
        .unwrap_or_else(|| "—".into())
}

/// Translate native pressure levels into words that describe the operating
/// condition. Color remains a secondary visual cue; it is never the diagnosis
/// shown to the user.
fn pressure_state_label(sample: &Sample) -> &'static str {
    match sample.pressure.as_str() {
        "GREEN" => "normal",
        "YELLOW" => "watch",
        "RED" => "critical",
        _ => match sample.pressure_meaning.as_str() {
            "normal" => "normal",
            "warning" => "watch",
            "critical" => "critical",
            _ => "unavailable",
        },
    }
}

fn gpu_load_label(value: Option<u8>, thresholds: Thresholds) -> &'static str {
    match value.map(u64::from) {
        Some(value) if value >= thresholds.gpu_critical_load => "saturated",
        Some(value) if value >= thresholds.gpu_warn_load => "loaded",
        Some(_) => "within target",
        None => "unavailable",
    }
}

/// Report measured GPU load transitions without declaring a critical fault.
/// Missing readings cannot establish that a busy period ended.
fn gpu_journal_transition(
    previous: Option<u8>,
    current: Option<u8>,
    thresholds: Thresholds,
) -> Option<(String, Tone)> {
    let (previous, current) = (u64::from(previous?), u64::from(current?));
    let saturated = current >= thresholds.gpu_critical_load;
    let saturation_changed = (previous >= thresholds.gpu_critical_load) != saturated;
    // Keep the existing busy-burst threshold to avoid logging every 75↔74%
    // fluctuation. Per-sample chart bands still retain all measured changes.
    let busy = current >= GPU_BUSY_ENTER_LOAD;
    let busy_changed = (previous >= GPU_BUSY_ENTER_LOAD) != busy;
    if !saturation_changed && !busy_changed {
        return None;
    }
    let label = if current == 0 {
        "GPU idle"
    } else if saturation_changed && saturated {
        "GPU load saturated"
    } else if !saturation_changed && busy {
        "busy burst started"
    } else {
        "GPU load eased"
    };
    Some((
        format!("{label} · {current}% busy"),
        ChartMetric::Gpu.tone(current, thresholds),
    ))
}

fn telemetry_source(sample: &Sample) -> String {
    let label = match sample.llm_source {
        TelemetrySource::Live => "LIVE",
        TelemetrySource::Log => "LOG",
        TelemetrySource::Report => "REPORTED",
        TelemetrySource::None => "SOURCE —",
    };
    format!("{label} · {}", telemetry_age(sample.llm_observed_at))
}

fn telemetry_age(observed_at: Option<SystemTime>) -> String {
    let Some(observed_at) = observed_at else {
        return "age —".into();
    };
    let seconds = SystemTime::now()
        .duration_since(observed_at)
        .map(|age| age.as_secs())
        .unwrap_or(0);
    if seconds < 60 {
        format!("{seconds}s old")
    } else if seconds < 3600 {
        format!("{}m old", seconds / 60)
    } else {
        format!("{}h old", seconds / 3600)
    }
}

fn count(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".into())
}

fn process_count_label(sample: &Sample) -> String {
    if sample.llm_count == 1 {
        "1 process".into()
    } else {
        format!("{} processes", sample.llm_count)
    }
}

impl CorrelationEngine {
    fn observe(&mut self, sample: &Sample, thresholds: Thresholds) -> CorrelationInsight {
        if sample.llm_remote {
            self.observations.clear();
            return CorrelationInsight::default();
        }
        let current = CorrelationObservation::from_sample(sample);
        let previous = self.observations.back().cloned();
        let comparable_previous = previous
            .as_ref()
            .filter(|previous| {
                previous.provider == current.provider && previous.model == current.model
            })
            .cloned();
        let baseline = self.baseline(&current.provider, &current.model);
        let insight =
            correlate_observations(&current, comparable_previous.as_ref(), baseline, thresholds);

        self.observations.push_back(current);
        while self.observations.len() > CORRELATION_HISTORY_LIMIT {
            self.observations.pop_front();
        }
        insight
    }

    fn baseline(&self, provider: &str, model: &str) -> Option<f64> {
        let mut rates = self
            .observations
            .iter()
            .filter(|observation| observation.provider == provider && observation.model == model)
            .filter_map(|observation| observation.generation_tps)
            .filter(|value| value.is_finite() && *value > 0.0)
            .collect::<Vec<_>>();
        if rates.is_empty() {
            return None;
        }
        rates.sort_by(f64::total_cmp);
        let middle = rates.len() / 2;
        if rates.len().is_multiple_of(2) {
            Some((rates[middle - 1] + rates[middle]) / 2.0)
        } else {
            Some(rates[middle])
        }
    }

    fn reset(&mut self) {
        self.observations.clear();
    }
}

impl CorrelationObservation {
    fn from_sample(sample: &Sample) -> Self {
        Self {
            provider: sample.llm_provider.clone(),
            model: sample.llm_model.clone(),
            generation_tps: sample
                .llm_generation_tps_live
                .then_some(sample.llm_generation_tps)
                .flatten(),
            gpu_util: sample.gpu_util,
            renderer_util: sample.metal.renderer_util,
            tiler_util: sample.metal.tiler_util,
            paging_rate: sample.swap_in.saturating_add(sample.swap_out),
            compression_rate: sample.compress.saturating_add(sample.decompress),
            pressure: pressure_rank(&sample.pressure),
            thermal_limited: sample.thermal.starts_with("limited")
                || sample.thermal == "warning reported",
            model_memory: sample.llm_model_memory,
            model_memory_max: sample.llm_model_memory_max,
            metal_in_use: sample.gpu_in_use,
            metal_alloc: sample.gpu_alloc,
            context_tokens: llm_context_tokens(sample),
            active_requests: sample.llm_active_requests,
            waiting_requests: sample.llm_waiting_requests,
        }
    }
}

struct CorrelationFactor {
    cause: CorrelationCause,
    score: u8,
    evidence: String,
}

fn correlate_observations(
    current: &CorrelationObservation,
    previous: Option<&CorrelationObservation>,
    baseline: Option<f64>,
    thresholds: Thresholds,
) -> CorrelationInsight {
    let current_tps = current.generation_tps.filter(|value| {
        value.is_finite() && (*value > 0.0 || current.active_requests.unwrap_or_default() > 0)
    });
    let baseline = baseline.filter(|value| value.is_finite() && *value > 0.0);
    let (direction, delta_percent) = match (current_tps, baseline) {
        (Some(current), Some(baseline)) => {
            let delta = current - baseline;
            let percent = delta / baseline * 100.0;
            let direction = if delta <= -THROUGHPUT_CHANGE_MIN_TPS
                && percent <= -(THROUGHPUT_CHANGE_RATIO * 100.0)
            {
                ThroughputDirection::Down
            } else if delta >= THROUGHPUT_CHANGE_MIN_TPS
                && percent >= THROUGHPUT_CHANGE_RATIO * 100.0
            {
                ThroughputDirection::Up
            } else {
                ThroughputDirection::Flat
            };
            (direction, Some(percent))
        }
        _ => (ThroughputDirection::Unknown, None),
    };

    let mut factors = Vec::new();
    if current.thermal_limited {
        push_correlation_factor(&mut factors, CorrelationCause::Thermal, 95, "thermal limit");
    }

    match current.pressure {
        4 => push_correlation_factor(
            &mut factors,
            CorrelationCause::MemoryPressure,
            100,
            "memory pressure critical",
        ),
        2 => push_correlation_factor(
            &mut factors,
            CorrelationCause::MemoryPressure,
            72,
            "memory pressure watch",
        ),
        _ => {}
    }

    if current.paging_rate >= thresholds.swap_warn_rate {
        let score = if current.paging_rate >= thresholds.swap_critical_rate {
            100
        } else {
            88
        };
        push_correlation_factor(
            &mut factors,
            CorrelationCause::Paging,
            score,
            format!("paging {}", rate(current.paging_rate)),
        );
    }

    if current.compression_rate >= thresholds.compression_warn_rate {
        push_correlation_factor(
            &mut factors,
            CorrelationCause::Compression,
            68,
            format!("compression {}", rate(current.compression_rate)),
        );
    }

    if let Some(waiting) = current.waiting_requests.filter(|waiting| *waiting > 0) {
        let score = if waiting > 1 { 82 } else { 70 };
        push_correlation_factor(
            &mut factors,
            CorrelationCause::Queueing,
            score,
            format!(
                "queue {waiting} waiting · active {}",
                count(current.active_requests)
            ),
        );
    }

    let current_gpu = [current.gpu_util, current.renderer_util, current.tiler_util]
        .into_iter()
        .flatten()
        .max();
    if let Some(gpu) = current_gpu {
        if u64::from(gpu) >= thresholds.gpu_critical_load {
            let crossed = previous
                .and_then(|previous| {
                    [
                        previous.gpu_util,
                        previous.renderer_util,
                        previous.tiler_util,
                    ]
                    .into_iter()
                    .flatten()
                    .max()
                })
                .is_none_or(|value| u64::from(value) < thresholds.gpu_critical_load);
            push_correlation_factor(
                &mut factors,
                CorrelationCause::GpuSaturation,
                if crossed { 95 } else { 72 },
                format!("GPU {gpu}% busy"),
            );
        } else if u64::from(gpu) >= GPU_BUSY_ENTER_LOAD {
            push_correlation_factor(
                &mut factors,
                CorrelationCause::GpuSaturation,
                58,
                format!("GPU {gpu}% busy"),
            );
        }
    }

    let current_metal_ratio = fraction(current.metal_in_use, current.metal_alloc);
    if let Some(ratio) = current_metal_ratio.filter(|ratio| *ratio >= 0.90) {
        let crossed = previous
            .and_then(|previous| fraction(previous.metal_in_use, previous.metal_alloc))
            .is_none_or(|value| value < 0.90);
        let evidence = match (current.metal_in_use, current.metal_alloc) {
            (Some(used), Some(allocated)) => format!(
                "Metal MEM {:.0}% ({}/{})",
                ratio * 100.0,
                bytes(used),
                bytes(allocated)
            ),
            _ => format!("Metal MEM {:.0}%", ratio * 100.0),
        };
        push_correlation_factor(
            &mut factors,
            CorrelationCause::MetalMemory,
            if crossed { 90 } else { 68 },
            evidence,
        );
    }

    let model_memory_delta = previous.and_then(|previous| {
        current
            .model_memory
            .zip(previous.model_memory)
            .map(|(current, previous)| current.saturating_sub(previous))
    });
    let current_model_ratio = fraction(current.model_memory, current.model_memory_max);
    let mut model_memory_evidence = Vec::new();
    let mut model_memory_score = 0;
    if let Some(ratio) = current_model_ratio.filter(|ratio| *ratio >= 0.90) {
        model_memory_score = 84;
        model_memory_evidence.push(format!("model MEM {:.0}% of ceiling", ratio * 100.0));
    }
    if let Some(delta) = model_memory_delta.filter(|delta| *delta >= MODEL_MEMORY_GROWTH) {
        model_memory_score = model_memory_score.max(72);
        model_memory_evidence.push(format!("model MEM +{}", bytes(delta)));
    }
    if model_memory_score > 0 {
        push_correlation_factor(
            &mut factors,
            CorrelationCause::ModelMemory,
            model_memory_score,
            model_memory_evidence.join(" · "),
        );
    }

    let context_delta = previous.and_then(|previous| {
        current
            .context_tokens
            .zip(previous.context_tokens)
            .map(|(current, previous)| current.saturating_sub(previous))
    });
    let mut context_evidence = Vec::new();
    let mut context_score = 0;
    if let Some(delta) = context_delta.filter(|delta| *delta >= CONTEXT_GROWTH_TOKENS) {
        context_score = 82;
        if let Some(total) = current.context_tokens {
            context_evidence.push(format!(
                "context +{} → {}",
                compact_tokens(delta),
                compact_tokens(total)
            ));
        }
    } else if direction == ThroughputDirection::Down
        && current
            .context_tokens
            .is_some_and(|tokens| tokens >= 16_384)
    {
        context_score = 48;
        context_evidence.push(format!(
            "context {}",
            compact_tokens(current.context_tokens.unwrap_or_default())
        ));
    }
    if context_score > 0 {
        push_correlation_factor(
            &mut factors,
            CorrelationCause::ContextGrowth,
            context_score,
            context_evidence.join(" · "),
        );
    }

    if direction == ThroughputDirection::Down && factors.is_empty() {
        push_correlation_factor(
            &mut factors,
            CorrelationCause::Runtime,
            20,
            "no matching system signal; workload/runtime changed",
        );
    }
    factors.sort_by_key(|factor| std::cmp::Reverse(factor.score));

    let cause = factors
        .first()
        .map(|factor| factor.cause)
        .unwrap_or_default();
    let confidence = factors.first().map(|factor| factor.score).unwrap_or(0);
    let rate_label = match (current_tps, baseline, direction, delta_percent) {
        (Some(current), Some(baseline), ThroughputDirection::Down, Some(percent)) => format!(
            "GEN ↓{:.1}% ({baseline:.1}→{current:.1} tok/s)",
            percent.abs()
        ),
        (Some(current), Some(baseline), ThroughputDirection::Up, Some(percent)) => {
            format!("GEN ↑{percent:.1}% ({baseline:.1}→{current:.1} tok/s)")
        }
        (Some(current), _, _, _) => format!("GEN {current:.1} tok/s"),
        _ => String::new(),
    };
    let evidence = factors
        .iter()
        .take(2)
        .map(|factor| factor.evidence.as_str())
        .collect::<Vec<_>>()
        .join(" + ");
    let summary = if rate_label.is_empty() {
        String::new()
    } else if evidence.is_empty() {
        if direction == ThroughputDirection::Down {
            format!("{rate_label} · no matching system signal")
        } else {
            String::new()
        }
    } else {
        format!("{rate_label} · correlated: {evidence}")
    };
    let details = if rate_label.is_empty() || factors.is_empty() {
        String::new()
    } else {
        let all_evidence = factors
            .iter()
            .map(|factor| factor.evidence.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        format!("{} · {all_evidence}", cause.label())
    };
    let event_key =
        (direction == ThroughputDirection::Down).then_some(CorrelationKey { direction, cause });

    CorrelationInsight {
        direction,
        cause,
        confidence,
        summary,
        details,
        event_key,
    }
}

fn push_correlation_factor(
    factors: &mut Vec<CorrelationFactor>,
    cause: CorrelationCause,
    score: u8,
    evidence: impl Into<String>,
) {
    factors.push(CorrelationFactor {
        cause,
        score,
        evidence: evidence.into(),
    });
}

fn pressure_rank(pressure: &str) -> u8 {
    match pressure {
        "GREEN" => 1,
        "YELLOW" => 2,
        "RED" => 4,
        _ => 0,
    }
}

fn fraction(numerator: Option<u64>, denominator: Option<u64>) -> Option<f64> {
    let denominator = denominator.filter(|value| *value > 0)?;
    let numerator = numerator?;
    Some(numerator as f64 / denominator as f64)
}

fn llm_context_tokens(sample: &Sample) -> Option<u64> {
    match (sample.llm_prompt_tokens, sample.llm_output_tokens) {
        (Some(prompt), Some(output)) => Some(prompt.saturating_add(output)),
        (Some(prompt), None) => Some(prompt),
        // Output alone (including summed llama-server slots) is not context.
        (None, _) => None,
    }
}

fn compact_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn sample_macos_memory(host: &dyn Host, sample: &mut Sample, page_size: u64) {
    let level = host
        .command(
            "/usr/sbin/sysctl",
            &["-n", "kern.memorystatus_vm_pressure_level"],
        )
        .unwrap_or_default();
    match level.trim() {
        "1" => {
            sample.pressure = "GREEN".into();
            sample.pressure_meaning = "normal".into();
            sample.pressure_tone = Tone::Green;
        }
        "2" => {
            sample.pressure = "YELLOW".into();
            sample.pressure_meaning = "warning".into();
            sample.pressure_tone = Tone::Yellow;
        }
        "4" => {
            sample.pressure = "RED".into();
            sample.pressure_meaning = "critical".into();
            sample.pressure_tone = Tone::Red;
        }
        _ => {}
    }

    if let Some(output) = host.command("/usr/bin/memory_pressure", &["-Q"]) {
        if let Some(line) = output.lines().find(|line| line.contains("free percentage")) {
            sample.availability = line
                .split(|c: char| !c.is_ascii_digit())
                .find(|v| !v.is_empty())
                .and_then(|v| v.parse::<u8>().ok());
        }
    }

    let vm = host.command("/usr/bin/vm_stat", &[]).unwrap_or_default();
    sample.vm_available = !vm.trim().is_empty();
    let counters = parse_vm_stat(&vm, page_size);
    // vm_stat prints free_count minus speculative_count as "Pages free".
    // memory_pressure -Q uses AVAILABLE_NON_COMPRESSED_MEMORY, including
    // active and inactive application pages, so it cannot measure RAM use.
    sample.resident_memory = resident_memory_bytes(
        sample.total_memory,
        counters
            .free
            .zip(counters.speculative)
            .and_then(|(free, speculative)| free.checked_add(speculative)),
    );
    sample.wired = counters.wired;
    sample.compressor = counters.compressor;
    sample.compressed_logical = counters.compressed_logical;
    sample.anonymous = counters.anonymous;
    sample.file_backed = counters.file_backed;
    let swap_usage = host.command("/usr/sbin/sysctl", &["-n", "vm.swapusage"]);
    sample.swap_available = swap_usage.is_some();
    (sample.swap_total, sample.swap_used) = parse_swap_usage(&swap_usage.unwrap_or_default());
}

fn macos_counters_for_rates(host: &dyn Host, page_size: u64) -> VmCounters {
    let vm = host.command("/usr/bin/vm_stat", &[]).unwrap_or_default();
    parse_vm_stat(&vm, page_size)
}

fn sample_macos_gpu_thermal(host: &dyn Host, sample: &mut Sample) {
    let ioreg = host
        .command(
            "/usr/sbin/ioreg",
            &["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"],
        )
        .unwrap_or_default();
    (sample.gpu_util, sample.gpu_alloc, sample.gpu_in_use) = parse_gpu(&ioreg);
    let live_metal = parse_metal_hardware(&ioreg);
    sample.metal.device_name = live_metal.device_name.or(sample.metal.device_name.take());
    sample.metal.gpu_cores = live_metal.gpu_cores.or(sample.metal.gpu_cores);
    sample.metal.renderer_util = live_metal.renderer_util;
    sample.metal.tiler_util = live_metal.tiler_util;
    sample.thermal = parse_thermal(
        &host
            .command("/usr/bin/pmset", &["-g", "therm"])
            .unwrap_or_default(),
    );
}

// --- Linux sampling -------------------------------------------------------
// Linux has no vm_stat / ioreg / pmset. Memory and swap come from
// /proc/meminfo, paging rates from /proc/vmstat (pswpin/pswpout), pressure
// level from the MemAvailable ratio blended with /proc/pressure/memory
// stalls, GPUs from nvidia-smi when present, and thermals from
// /sys/class/thermal. Anything without a source stays `None`/unavailable
// and the UI already renders that as a dash.

#[derive(Default)]
struct LinuxMeminfo {
    total_kb: u64,
    free_kb: Option<u64>,
    available_kb: u64,
    swap_total_kb: u64,
    swap_free_kb: u64,
    anon_kb: u64,
    file_kb: u64,
}

fn parse_meminfo_value_kb(text: &str, key: &str) -> u64 {
    parse_meminfo_optional_kb(text, key).unwrap_or(0)
}

fn parse_meminfo_optional_kb(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.starts_with(key))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u64>().ok())
}

fn parse_linux_meminfo(text: &str) -> LinuxMeminfo {
    let total_kb = parse_meminfo_value_kb(text, "MemTotal:");
    let available_kb = parse_meminfo_value_kb(text, "MemAvailable:");
    let swap_total_kb = parse_meminfo_value_kb(text, "SwapTotal:");
    let swap_free_kb = parse_meminfo_value_kb(text, "SwapFree:");
    let anon_kb = parse_meminfo_value_kb(text, "Active(anon):")
        .saturating_add(parse_meminfo_value_kb(text, "Inactive(anon):"));
    let anon_fallback = if anon_kb == 0 {
        parse_meminfo_value_kb(text, "AnonPages:")
    } else {
        anon_kb
    };
    let file_kb = parse_meminfo_value_kb(text, "Active(file):")
        .saturating_add(parse_meminfo_value_kb(text, "Inactive(file):"))
        .saturating_add(parse_meminfo_value_kb(text, "Cached:"))
        .saturating_add(parse_meminfo_value_kb(text, "Buffers:"));
    LinuxMeminfo {
        total_kb,
        free_kb: parse_meminfo_optional_kb(text, "MemFree:"),
        available_kb,
        swap_total_kb,
        swap_free_kb,
        anon_kb: anon_fallback,
        file_kb,
    }
}

fn parse_linux_vmstat_value(text: &str, key: &str) -> u64 {
    text.lines()
        .find(|line| line.starts_with(key))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
}

fn parse_linux_paging(text: &str) -> (u64, u64) {
    (
        parse_linux_vmstat_value(text, "pswpin"),
        parse_linux_vmstat_value(text, "pswpout"),
    )
}

/// Stall average (`avg10`) of the `full` line in /proc/pressure/memory.
/// Returns a percentage in the 0–100 range, or `None` when unavailable.
fn parse_memory_pressure_stall(text: &str) -> Option<f64> {
    text.lines()
        .find(|line| line.starts_with("full"))?
        .split_whitespace()
        .find_map(|token| token.strip_prefix("avg10=")?.parse::<f64>().ok())
}

fn linux_pressure_state(
    load_percent: u64,
    stall_avg10: Option<f64>,
    thresholds: Thresholds,
) -> (&'static str, Tone) {
    // MemAvailable excludes reclaimable cache from load; resident occupancy
    // does not. Apply the configured bands here, then escalate for full stalls.
    let mut level = match ChartMetric::Memory.tone(load_percent, thresholds) {
        Tone::Red => 2,
        Tone::Yellow => 1,
        _ => 0,
    };
    match stall_avg10 {
        Some(stall) if stall >= 5.0 => level = level.max(2),
        Some(stall) if stall >= 1.0 => level = level.max(1),
        _ => {}
    }
    match level {
        2 => ("RED", Tone::Red),
        1 => ("YELLOW", Tone::Yellow),
        _ => ("GREEN", Tone::Green),
    }
}

fn linux_total_memory(host: &dyn Host) -> u64 {
    host.read_file(Path::new("/proc/meminfo"))
        .map(|text| parse_linux_meminfo(&text))
        .map(|info| info.total_kb.saturating_mul(1024))
        .unwrap_or(0)
}

fn linux_page_size(host: &dyn Host) -> u64 {
    host.command_u64("getconf", &["PAGESIZE"]).unwrap_or(4096)
}

fn linux_cpu_architecture(host: &dyn Host) -> Option<String> {
    host.command("uname", &["-m"])
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn linux_metal_init(host: &dyn Host) -> MetalTelemetry {
    MetalTelemetry {
        architecture: linux_cpu_architecture(host),
        ..MetalTelemetry::default()
    }
}

fn linux_thermal_celsius(host: &dyn Host, nvidia_temp: Option<u64>) -> Option<u64> {
    let mut hottest = nvidia_temp.unwrap_or(0);
    for entry in host.read_dir(Path::new("/sys/class/thermal")) {
        if let Some(text) = host.read_file(&entry.join("temp")) {
            // Values are millidegrees Celsius on most drivers.
            if let Ok(millidegrees) = text.trim().parse::<i64>() {
                if millidegrees > 0 {
                    hottest = hottest.max((millidegrees / 1000).max(0) as u64);
                }
            }
        }
    }
    (hottest > 0).then_some(hottest)
}

fn linux_thermal_label(hottest: Option<u64>) -> String {
    hottest
        .map(|value| format!("{value}°C measured"))
        .unwrap_or_else(|| "unavailable".into())
}

fn linux_counters_for_rates(host: &dyn Host) -> VmCounters {
    let mut counters = VmCounters::default();
    if let Some(text) = host.read_file(Path::new("/proc/vmstat")) {
        let (swapins, swapouts) = parse_linux_paging(&text);
        counters.swapins = swapins;
        counters.swapouts = swapouts;
    }
    counters
}

fn sample_linux_memory(
    host: &dyn Host,
    sample: &mut Sample,
    _page_size: u64,
    total_memory: u64,
    thresholds: Thresholds,
) {
    let Some(text) = host.read_file(Path::new("/proc/meminfo")) else {
        return;
    };
    let info = parse_linux_meminfo(&text);
    let total = if total_memory > 0 {
        total_memory
    } else {
        info.total_kb.saturating_mul(1024)
    };
    sample.total_memory = total;
    sample.resident_memory =
        resident_memory_bytes(total, info.free_kb.and_then(|free| free.checked_mul(1024)));
    sample.vm_available = true;
    sample.anonymous = info.anon_kb.saturating_mul(1024);
    sample.file_backed = info.file_kb.saturating_mul(1024);
    sample.wired = 0;
    sample.compressor = 0;
    sample.compressed_logical = 0;

    if total > 0 {
        let available = info.available_kb.saturating_mul(1024).min(total);
        let free_percent = (available.saturating_mul(100) / total).min(100);
        sample.availability = u8::try_from(free_percent).ok();
        let load = 100u64.saturating_sub(free_percent);
        let stall = host
            .read_file(Path::new("/proc/pressure/memory"))
            .as_deref()
            .and_then(parse_memory_pressure_stall);
        let (pressure, tone) = linux_pressure_state(load, stall, thresholds);
        sample.pressure = pressure.into();
        sample.pressure_meaning = match pressure {
            "GREEN" => "normal",
            "YELLOW" => "warning",
            "RED" => "critical",
            _ => "unavailable",
        }
        .into();
        sample.pressure_tone = tone;
    }

    sample.swap_total = info.swap_total_kb.saturating_mul(1024);
    sample.swap_used = info
        .swap_total_kb
        .saturating_sub(info.swap_free_kb.min(info.swap_total_kb))
        .saturating_mul(1024);
    sample.swap_available = true;
}

fn sample_linux_gpu_thermal(host: &dyn Host, sample: &mut Sample, previous: &[gpu::Device]) {
    sample.gpus = gpu::collect(host, previous);
    sample.gpu_util = gpu::peak_utilization(&sample.gpus);
    // VRAM remains per-card. A sum would falsely suggest a single allocation
    // pool and feed NVIDIA memory into the Apple Metal correlation rules.
    sample.gpu_in_use = None;
    sample.gpu_alloc = None;
    sample.metal.renderer_util = None;
    sample.metal.tiler_util = None;
    let hottest_gpu = sample.gpus.iter().filter_map(|gpu| gpu.temperature).max();
    sample.thermal = linux_thermal_label(linux_thermal_celsius(host, hottest_gpu));
}

fn parse_vm_stat(text: &str, page_size: u64) -> VmCounters {
    let mut c = VmCounters::default();
    for line in text.lines() {
        let Some(value) = line
            .split(':')
            .nth(1)
            .and_then(|v| {
                v.split_whitespace()
                    .next()
                    .map(|value| value.trim_matches(|c: char| !c.is_ascii_digit()))
            })
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        let bytes = value.saturating_mul(page_size);
        if line.starts_with("Pages free:") {
            c.free = value.checked_mul(page_size);
        } else if line.starts_with("Pages speculative:") {
            c.speculative = value.checked_mul(page_size);
        } else if line.starts_with("Pages wired") {
            c.wired = bytes;
        } else if line.starts_with("Pages occupied by compressor")
            || line.starts_with("Pages used by compressor")
        {
            c.compressor = bytes;
        } else if line.starts_with("Pages stored in compressor")
            || line.starts_with("Uncompressed pages")
        {
            c.compressed_logical = bytes;
        } else if line.starts_with("Anonymous pages") {
            c.anonymous = bytes;
        } else if line.starts_with("File-backed pages") {
            c.file_backed = bytes;
        } else if line.starts_with("Swapins") || line.contains("\"Swapins\"") {
            c.swapins = value;
        } else if line.starts_with("Swapouts") || line.contains("\"Swapouts\"") {
            c.swapouts = value;
        } else if line.starts_with("Compressions") || line.starts_with("Pages compressed") {
            c.compressions = value;
        } else if line.starts_with("Decompressions") || line.starts_with("Pages decompressed") {
            c.decompressions = value;
        } else if line.starts_with("Pages reactivated") {
            c.reactivations = value;
        }
    }
    c
}

fn resident_memory_bytes(total: u64, free: Option<u64>) -> Option<u64> {
    (total > 0).then_some(())?;
    total.checked_sub(free?)
}

fn resident_memory_percent(sample: &Sample) -> Option<u64> {
    let used = sample.resident_memory?;
    if sample.total_memory == 0 || used > sample.total_memory {
        return None;
    }
    Some((u128::from(used) * 100 / u128::from(sample.total_memory)) as u64)
}

fn parse_swap_usage(text: &str) -> (u64, u64) {
    let mut total = 0;
    let mut used = 0;
    let normalized = text.replace('=', " = ");
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        if (tokens[i] == "total" || tokens[i] == "used") && i + 1 < tokens.len() {
            let value_index = if tokens[i + 1] == "=" { i + 2 } else { i + 1 };
            if let Some(value) = tokens.get(value_index) {
                if tokens[i] == "total" {
                    total = parse_unit(value);
                } else {
                    used = parse_unit(value);
                }
                i = value_index;
            }
        }
        i += 1;
    }
    (total, used)
}

fn parse_gpu(text: &str) -> (Option<u8>, Option<u64>, Option<u64>) {
    (
        find_named_number(text, "Device Utilization %")
            .or_else(|| find_named_number(text, "Renderer Utilization %"))
            .map(|v| v as u8),
        find_named_numbers(text, "Alloc system memory")
            .into_iter()
            .next_back(),
        find_named_numbers(text, "In use system memory")
            .into_iter()
            .next_back(),
    )
}

fn parse_metal_hardware(text: &str) -> MetalTelemetry {
    MetalTelemetry {
        device_name: find_named_string(text, "model")
            .or_else(|| find_named_string(text, "MetalPluginName")),
        architecture: None,
        gpu_cores: find_named_number(text, "gpu-core-count")
            .and_then(|value| value.try_into().ok()),
        renderer_util: find_named_number(text, "Renderer Utilization %")
            .and_then(|value| value.try_into().ok()),
        tiler_util: find_named_number(text, "Tiler Utilization %")
            .and_then(|value| value.try_into().ok()),
        resource_limit: None,
    }
}

fn parse_thermal(text: &str) -> String {
    if let Some(value) = find_number(text, "CPU_Speed_Limit") {
        if value < 100 {
            return format!("limited {value}%");
        }
        return "no limit".into();
    }
    if text.is_empty() {
        "unavailable".into()
    } else if text.contains("No thermal warning") || text.contains("No performance warning") {
        "no warning".into()
    } else {
        "warning reported".into()
    }
}

fn parse_processes(text: &str) -> ProcessSnapshot {
    let mut llm_count = 0;
    let mut llm_rss = 0;
    let mut llm_cpu = 0.0;
    let mut provider: Option<String> = None;
    let mut consumers: Vec<Consumer> = Vec::new();
    let mut llm_processes: Vec<LlmProcess> = Vec::new();

    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let Ok(pid) = fields[0].parse::<u32>() else {
            continue;
        };
        let Ok(rss_kib) = fields[1].parse::<u64>() else {
            continue;
        };
        let Ok(cpu) = fields[2].parse::<f64>() else {
            continue;
        };
        let modern_memory_percent = fields
            .get(3)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0);
        let (memory_percent, state, pageins, name_index, command_index) =
            if modern_memory_percent.is_some() {
                (
                    modern_memory_percent,
                    fields.get(4).copied().unwrap_or("?").to_string(),
                    fields.get(5).and_then(|value| value.parse::<u64>().ok()),
                    6,
                    7,
                )
            } else {
                (None, "?".into(), None, 3, 4)
            };
        let Some(name_field) = fields.get(name_index) else {
            continue;
        };
        let name = name_field
            .rsplit('/')
            .next()
            .unwrap_or(name_field)
            .to_string();
        let command = fields
            .get(command_index..)
            .map(|parts| parts.join(" "))
            .unwrap_or_else(|| name.clone());
        let lower = line.to_ascii_lowercase();
        let is_llm = is_llm_process(&name, &command);
        if is_llm {
            if provider.is_none() {
                provider = process_provider(&name, &command);
            }
            llm_count += 1;
            llm_rss += rss_kib * 1024;
            llm_cpu += cpu;
            llm_processes.push(LlmProcess {
                pid,
                name: name.clone(),
                command: command.clone(),
                rss: rss_kib * 1024,
                cpu,
                memory_percent,
                state,
                pageins,
                pagein_rate: None,
            });
        }

        if lower.contains("mlxtop") || name == "ps" || name == "awk" {
            continue;
        }
        if let Some(consumer) = consumers.iter_mut().find(|c| c.name == name) {
            consumer.rss += rss_kib * 1024;
            consumer.processes += 1;
        } else {
            consumers.push(Consumer {
                name,
                rss: rss_kib * 1024,
                processes: 1,
            });
        }
    }
    consumers.sort_by_key(|consumer| std::cmp::Reverse(consumer.rss));
    let largest_consumer = consumers.first().map(|consumer| consumer.name.clone());
    llm_processes.sort_by_key(|process| std::cmp::Reverse(process.rss));
    llm_processes.truncate(32);
    let top_llm = llm_processes.first().cloned();
    ProcessSnapshot {
        llm_count,
        llm_rss,
        llm_cpu,
        top_llm,
        provider,
        largest_consumer,
        llm_processes,
    }
}

fn annotate_process_pagein_rates(
    processes: &mut [LlmProcess],
    previous: &[LlmProcess],
    elapsed: Duration,
) {
    let seconds = elapsed.as_secs_f64().max(0.001);
    for process in processes {
        process.pagein_rate = process.pageins.and_then(|current| {
            previous
                .iter()
                .find(|old| old.pid == process.pid)
                .and_then(|old| old.pageins)
                .map(|old| delta(current, old) as f64 / seconds)
        });
    }
}

fn is_llm_process(name: &str, command: &str) -> bool {
    process_provider(name, command).is_some()
}

fn normalize_process_token(value: &str) -> String {
    value
        .trim_matches(['"', '\''])
        .rsplit('/')
        .next()
        .unwrap_or(value)
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn process_provider(name: &str, command: &str) -> Option<String> {
    let command_tokens = command
        .split_whitespace()
        .take(4)
        .take_while(|token| !token.starts_with("--"))
        .collect::<Vec<_>>();
    let prefix = command_tokens.join(" ").to_ascii_lowercase();
    let candidates: Vec<_> = std::iter::once(name)
        .chain(command_tokens.iter().copied())
        .map(normalize_process_token)
        .filter(|token| !token.contains("mlxtop"))
        .collect();
    let has = |marker: &str| candidates.iter().any(|token| token.contains(marker));
    // Bionic uses LM Studio's runtime but has a different app executable name.
    // Match the executable's bundle path, not unrelated uses of "bionic".
    let is_bionic = std::iter::once(name)
        .chain(command_tokens.first().copied())
        .any(|token| {
            token
                .trim_matches(['"', '\''])
                .to_ascii_lowercase()
                .ends_with("/bionic.app/contents/macos/bionic")
        });
    // LM Studio can host a llama-server worker; retain the owning runtime.
    let provider = if has("lmstudio")
        || has("llmster")
        || prefix.contains("lm studio")
        || prefix.contains(".lmstudio/")
        || is_bionic
    {
        "LM Studio"
    } else if has("omlx") {
        "oMLX"
    } else if has("ollama") {
        "Ollama"
    } else if has("koboldcpp") {
        "KoboldCpp"
    } else if has("localai") {
        "LocalAI"
    } else if has("vllm") {
        "vLLM"
    } else if has("sglang") {
        "SGLang"
    } else if has("gpt4all") {
        "GPT4All"
    } else if candidates
        .iter()
        .any(|token| token == "jan" || token == "janexe")
        || prefix.contains("/jan.app/")
        || prefix.contains("/.jan/")
    {
        "Jan"
    } else if has("llamaserver") || has("llamacpp") {
        "llama.cpp"
    } else if has("mlxlm") {
        "mlx-lm"
    } else {
        return None;
    };
    Some(provider.into())
}

impl LlmTelemetryClient {
    fn from_config(config: &Config, home: Option<PathBuf>) -> Self {
        let (host, port) = read_omlx_endpoint(config, home.as_deref());
        Self {
            provider_adapter: providers::Adapter::new(),
            home,
            host,
            port,
            session_cookie: None,
            cached: None,
            mlx_metadata: MlxTelemetry::default(),
            last_stats_available: None,
            next_metadata_poll: Instant::now(),
            next_poll: Instant::now(),
            retry_backoff: Duration::from_secs(1),
        }
    }

    fn poll(&mut self, detected_provider: Option<&str>) -> Option<LlmTelemetry> {
        if self.provider_adapter.selected(detected_provider) {
            return self.provider_adapter.poll();
        }
        let now = Instant::now();
        if now < self.next_poll {
            return self.cached.clone();
        }
        let telemetry = self.poll_once();
        if let Some(mut telemetry) = telemetry {
            telemetry.remote = !is_loopback_host(&self.host);
            self.cached = Some(telemetry);
            self.retry_backoff = Duration::from_secs(1);
            self.next_poll = now + self.retry_backoff;
        } else {
            diagnostics_log(
                "WARN",
                "llm_api_poll_failed",
                format!(
                    "host={} port={} retry_seconds={}",
                    log_field(&self.host),
                    self.port,
                    self.retry_backoff.as_secs()
                ),
            );
            self.next_poll = now + self.retry_backoff;
            self.retry_backoff = (self.retry_backoff * 2).min(Duration::from_secs(30));
        }
        self.cached.clone()
    }

    fn poll_once(&mut self) -> Option<LlmTelemetry> {
        let health_response = match http_request(&self.host, self.port, "GET", "/health", &[], None)
        {
            Some(response) => response,
            None => {
                self.last_stats_available = None;
                diagnostics_log(
                    "WARN",
                    "llm_health_unreachable",
                    format!("host={} port={}", log_field(&self.host), self.port),
                );
                return None;
            }
        };
        if health_response.status != 200 {
            self.last_stats_available = None;
            diagnostics_log(
                "WARN",
                "llm_health_http_error",
                format!(
                    "host={} port={} status={}",
                    log_field(&self.host),
                    self.port,
                    health_response.status
                ),
            );
            return None;
        }
        let health: Value = match serde_json::from_str(&health_response.body) {
            Ok(health) => health,
            Err(error) => {
                self.last_stats_available = None;
                diagnostics_log(
                    "WARN",
                    "llm_health_invalid_json",
                    format!(
                        "host={} port={} error={}",
                        log_field(&self.host),
                        self.port,
                        log_field(&error.to_string())
                    ),
                );
                return None;
            }
        };
        if health.get("default_model").is_none() && health.get("engine_pool").is_none() {
            self.last_stats_available = None;
            diagnostics_log(
                "WARN",
                "llm_health_unrecognized",
                format!("host={} port={}", log_field(&self.host), self.port),
            );
            return None;
        }

        let stats = self.fetch_stats();
        let stats_available = stats.is_some();
        if self.last_stats_available != Some(stats_available) {
            diagnostics_log(
                if stats_available { "INFO" } else { "WARN" },
                "llm_api_stats",
                format!(
                    "host={} port={} available={stats_available}",
                    log_field(&self.host),
                    self.port
                ),
            );
            self.last_stats_available = Some(stats_available);
        }
        let now = Instant::now();
        if now >= self.next_metadata_poll {
            let device_info = self.fetch_json("/admin/api/device-info");
            let settings = self
                .fetch_json("/admin/api/global-settings")
                .or_else(|| self.fetch_json("/admin/api/settings"));
            let metadata = parse_mlx_metadata(device_info.as_ref(), settings.as_ref());
            let metadata_available = !mlx_metadata_is_empty(&metadata);
            self.mlx_metadata = merge_mlx_telemetry(&self.mlx_metadata, &metadata);
            self.next_metadata_poll = now
                + if metadata_available {
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(10)
                };
        }
        let mut telemetry = parse_omlx_telemetry(&health, stats.as_ref());
        telemetry.mlx = merge_mlx_telemetry(
            &self.mlx_metadata,
            &parse_mlx_runtime_telemetry(&health, stats.as_ref()),
        );
        telemetry.observed_at = Some(SystemTime::now());
        Some(telemetry)
    }

    fn fetch_stats(&mut self) -> Option<Value> {
        self.fetch_json("/admin/api/stats?scope=session")
    }

    fn fetch_json(&mut self, path: &str) -> Option<Value> {
        if self.session_cookie.is_none() {
            self.login();
        }
        let cookie = self.session_cookie.clone()?;
        let response = http_request(
            &self.host,
            self.port,
            "GET",
            path,
            &[("Cookie", cookie.as_str())],
            None,
        )?;
        if response.status == 401 {
            self.session_cookie = None;
            self.login();
            let cookie = self.session_cookie.clone()?;
            let response = http_request(
                &self.host,
                self.port,
                "GET",
                path,
                &[("Cookie", cookie.as_str())],
                None,
            )?;
            if response.status != 200 {
                return None;
            }
            return serde_json::from_str(&response.body).ok();
        }
        if response.status != 200 {
            return None;
        }
        serde_json::from_str(&response.body).ok()
    }

    fn login(&mut self) {
        if !is_loopback_host(&self.host)
            && env::var("MLXTOP_ALLOW_REMOTE_AUTH").as_deref() != Ok("1")
        {
            return;
        }
        let Some(api_key) = read_omlx_api_key(self.home.as_deref()) else {
            return;
        };
        let body = json!({ "api_key": api_key, "remember": true }).to_string();
        let Some(response) = http_request(
            &self.host,
            self.port,
            "POST",
            "/admin/api/login",
            &[("Content-Type", "application/json")],
            Some(&body),
        ) else {
            return;
        };
        if response.status == 200 {
            self.session_cookie = response
                .header("set-cookie")
                .and_then(|value| value.split(';').next())
                .map(str::to_owned);
        }
    }
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn http_request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<HttpResponse> {
    let address = (host, port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(250)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(250)))
        .ok()?;
    let body = body.unwrap_or("");
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n");
    for (key, value) in headers {
        request.push_str(key);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    if !body.is_empty() {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).ok()?;

    let mut raw = Vec::new();
    stream
        .take((MAX_HTTP_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut raw)
        .ok()?;
    if raw.len() > MAX_HTTP_RESPONSE_BYTES {
        return None;
    }
    let raw = String::from_utf8_lossy(&raw);
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let mut lines = head.lines();
    let status = lines
        .next()?
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())?;
    let headers = lines
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_owned(), value.trim().to_owned()))
        })
        .collect();
    Some(HttpResponse {
        status,
        headers,
        body: body.to_owned(),
    })
}

/**
 * Endpoint values discovered from `~/.config/omlx-coding/server.env`.
 *
 * `None` means the file said nothing usable about that field, which keeps
 * "absent" distinct from "explicitly set to the built-in default".
 */
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DiscoveredEndpoint {
    host: Option<String>,
    port: Option<u16>,
}

fn parse_omlx_server_env(text: &str) -> DiscoveredEndpoint {
    let mut discovered = DiscoveredEndpoint::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "HOST" | "OMLX_HOST" if !value.is_empty() && value != "0.0.0.0" => {
                discovered.host = Some(value.to_owned());
            }
            "PORT" | "OMLX_PORT" => {
                if let Ok(port) = value.parse::<u16>() {
                    discovered.port = Some(port);
                }
            }
            _ => {}
        }
    }
    discovered
}

/**
 * Resolve the endpoint to monitor.
 *
 * Discovery supplies the defaults; an explicitly configured host or port
 * always wins, because the user picked it deliberately. Each field is
 * resolved on its own, so configuring only a port keeps the discovered host.
 * A field that is neither configured nor discovered falls back to the
 * built-in default.
 */
fn resolve_omlx_endpoint(discovered: &DiscoveredEndpoint, config: &Config) -> (String, u16) {
    let configured = config.omx.as_ref();
    let host = configured
        .and_then(|omx| omx.host.clone())
        .filter(|host| !host.trim().is_empty())
        .or_else(|| discovered.host.clone())
        .unwrap_or_else(|| DEFAULT_OMLX_HOST.to_owned());
    let port = configured
        .and_then(|omx| omx.port)
        .or(discovered.port)
        .unwrap_or(DEFAULT_OMLX_PORT);
    (host, port)
}

fn read_omlx_endpoint(config: &Config, home: Option<&Path>) -> (String, u16) {
    let discovered = home
        .map(|home| home.join(".config/omlx-coding/server.env"))
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|text| parse_omlx_server_env(&text))
        .unwrap_or_default();
    let (host, port) = resolve_omlx_endpoint(&discovered, config);
    if discovered
        .host
        .as_deref()
        .is_some_and(|value| value != host)
        || discovered.port.is_some_and(|value| value != port)
    {
        diagnostics_log(
            "INFO",
            "omlx_endpoint_override",
            format!(
                "configured={host}:{port} discovered={}:{}",
                discovered.host.as_deref().unwrap_or("-"),
                discovered
                    .port
                    .map(|port| port.to_string())
                    .unwrap_or_else(|| "-".to_owned())
            ),
        );
    }
    (host, port)
}

fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.trim_matches(['[', ']']),
        "127.0.0.1" | "localhost" | "::1"
    )
}

fn read_omlx_api_key(home: Option<&Path>) -> Option<String> {
    let path = home?.join(".config/omlx-coding/server.env");
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| {
            let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
            let (key, value) = line.split_once('=')?;
            (key.trim() == "API_KEY").then(|| value.trim().trim_matches('"').to_owned())
        })
        .filter(|value| !value.is_empty())
}

fn parse_omlx_telemetry(health: &Value, stats: Option<&Value>) -> LlmTelemetry {
    let mut telemetry = LlmTelemetry {
        source: TelemetrySource::Live,
        provider: Some("oMLX".into()),
        status: Some(json_string(health, &["status"]).unwrap_or_else(|| "healthy".into())),
        model: json_string(health, &["default_model"]),
        model_memory: json_u64(health, &["engine_pool", "current_model_memory"]),
        model_memory_max: json_u64(health, &["engine_pool", "final_ceiling"]),
        mlx: parse_mlx_runtime_telemetry(health, stats),
        ..LlmTelemetry::default()
    };

    let Some(stats) = stats else {
        return telemetry;
    };
    telemetry.generation_tps =
        json_f64(stats, &["avg_generation_tps"]).filter(|value| *value >= 0.0);
    telemetry.prefill_tps = json_f64(stats, &["avg_prefill_tps"]).filter(|value| *value >= 0.0);
    telemetry.cache_efficiency =
        json_f64(stats, &["cache_efficiency"]).map(|value| value.clamp(0.0, 100.0));
    telemetry.requests = providers::omlx_requests(stats);
    telemetry.total_prompt_tokens = json_u64(stats, &["total_prompt_tokens"]);
    telemetry.total_cached_tokens = json_u64(stats, &["total_cached_tokens"]);
    telemetry.model_memory =
        json_u64(stats, &["active_models", "model_memory_used"]).or(telemetry.model_memory);
    telemetry.model_memory_max =
        json_u64(stats, &["active_models", "model_memory_max"]).or(telemetry.model_memory_max);

    // Headline values describe the whole server: loaded models share it, and
    // one model's queue or first request is not the server's.
    let models = stats
        .pointer("/active_models/models")
        .and_then(Value::as_array);
    let model_requests: Vec<_> = models
        .into_iter()
        .flatten()
        .map(|model| (model, providers::omlx_model_requests(model)))
        .collect();
    let busy: Vec<&Value> = model_requests
        .iter()
        .filter(|(model, requests)| {
            !requests.is_empty()
                || json_u64(model, &["active_requests"]).unwrap_or(0) > 0
                || json_u64(model, &["waiting_requests"]).unwrap_or(0) > 0
        })
        .map(|(model, _)| *model)
        .collect();
    let headline = busy
        .first()
        .copied()
        .or_else(|| models.and_then(|models| models.first()));
    let headline_id = headline.and_then(|model| json_string(model, &["id"]));
    if let Some(id) = &headline_id {
        // A distinct label also keeps multi-model throughput out of a single
        // model's correlation baseline.
        telemetry.model = Some(if busy.len() > 1 {
            format!("{} models · {id}", busy.len())
        } else {
            id.clone()
        });
    }
    let requests: Vec<&providers::OmlxRequest> = model_requests
        .iter()
        .flat_map(|(_, requests)| requests)
        .collect();
    let in_phase = |phase| -> Vec<&providers::OmlxRequest> {
        requests
            .iter()
            .copied()
            .filter(|request| request.phase == phase)
            .collect()
    };
    let generating = in_phase(providers::OmlxPhase::Generating);
    let prefilling = in_phase(providers::OmlxPhase::Prefilling);
    let waiting = in_phase(providers::OmlxPhase::Waiting);

    if let Some(models) = models {
        let total = |total: &str, per_model: &str| {
            json_u64(stats, &["active_models", total]).or_else(|| {
                models.iter().try_fold(0_u64, |sum, model| {
                    sum.checked_add(json_u64(model, &[per_model])?)
                })
            })
        };
        telemetry.active_requests = total("total_active_requests", "active_requests");
        telemetry.waiting_requests = total("total_waiting_requests", "waiting_requests");
    }
    // Concurrent requests add up; a partial sum is not presented as complete.
    let live_sum = |requests: &[&providers::OmlxRequest]| {
        (!requests.is_empty())
            .then(|| {
                requests
                    .iter()
                    .try_fold(0.0, |sum, request| Some(sum + request.rate?))
            })
            .flatten()
    };
    if let Some(rate) = live_sum(&generating) {
        telemetry.generation_tps = Some(rate);
        telemetry.generation_tps_live = true;
    }
    if let Some(rate) = live_sum(&prefilling) {
        telemetry.prefill_tps = Some(rate);
        telemetry.prefill_tps_live = true;
    }
    telemetry.output_tokens = (!generating.is_empty())
        .then(|| {
            generating
                .iter()
                .try_fold(0_u64, |sum, request| sum.checked_add(request.output?))
        })
        .flatten();
    // A prompt size describes one request; concurrent requests have none.
    let active = generating.len() + prefilling.len();
    telemetry.prompt_tokens = match (active, waiting.as_slice()) {
        (1, _) => generating
            .first()
            .or_else(|| prefilling.first())
            .and_then(|request| request.prompt),
        (0, [request]) => request.prompt.filter(|prompt| *prompt > 0),
        _ => None,
    };
    if !model_requests.is_empty() {
        let loading = model_requests.iter().any(|(model, _)| {
            model
                .get("is_loading")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });
        // Serving work outranks another model's load.
        telemetry.status = Some(
            if !prefilling.is_empty() {
                "prefilling"
            } else if !generating.is_empty() {
                "generating"
            } else if loading {
                "loading"
            } else if telemetry.waiting_requests.unwrap_or(0) > 0 {
                "waiting"
            } else if telemetry.active_requests.unwrap_or(0) > 0 {
                "active"
            } else {
                "idle"
            }
            .into(),
        );
    }

    // Prefix reuse is per model, so several busy models have no single rate.
    // Older responses list one cache entry without a model ID.
    let model_caches = stats
        .pointer("/runtime_cache/models")
        .and_then(Value::as_array);
    let model_cache = model_caches.and_then(|caches| {
        if busy.len() > 1 {
            return None;
        }
        caches
            .iter()
            .find(|cache| headline_id.is_some() && json_string(cache, &["id"]) == headline_id)
            .or_else(|| match caches.as_slice() {
                [cache] if cache.get("id").is_none() => Some(cache),
                _ => None,
            })
    });
    if let Some(model_cache) = model_cache {
        telemetry.prefix_hit_rate = json_f64(
            model_cache,
            &["cache_rates", "cumulative", "prefix_hit_rate"],
        )
        .map(|value| (value * 100.0).clamp(0.0, 100.0));
    }
    telemetry
}

fn parse_mlx_runtime_telemetry(health: &Value, stats: Option<&Value>) -> MlxTelemetry {
    let mut sources = vec![health];
    if let Some(stats) = stats {
        sources.push(stats);
    }
    parse_mlx_sources(&sources)
}

fn parse_mlx_metadata(device_info: Option<&Value>, settings: Option<&Value>) -> MlxTelemetry {
    let mut sources = Vec::new();
    if let Some(device_info) = device_info {
        sources.push(device_info);
    }
    if let Some(settings) = settings {
        sources.push(settings);
    }
    parse_mlx_sources(&sources)
}

fn parse_mlx_sources(sources: &[&Value]) -> MlxTelemetry {
    let mut telemetry = MlxTelemetry::default();
    for source in sources {
        let active_memory = json_u64_paths_or_keys(
            source,
            &[
                &["mlx_memory", "active_bytes"],
                &["mlx", "active_bytes"],
                &["mlx", "active_memory"],
                &["active_memory_bytes"],
            ],
            &["mlx_active_memory_bytes"],
        );
        let cache_memory = json_u64_paths_or_keys(
            source,
            &[
                &["mlx_memory", "cache_bytes"],
                &["mlx", "cache_bytes"],
                &["mlx", "cache_memory"],
                &["cache_memory_bytes"],
            ],
            &["mlx_cache_memory_bytes"],
        );
        let peak_memory = json_u64_paths_or_keys(
            source,
            &[
                &["mlx_memory", "peak_bytes"],
                &["mlx", "peak_bytes"],
                &["mlx", "peak_memory"],
                &["peak_memory_bytes"],
            ],
            &["mlx_peak_memory_bytes"],
        );
        let process_footprint = json_u64_paths_or_keys(
            source,
            &[
                &["system", "omlx_phys_footprint_bytes"],
                &["system", "phys_footprint_bytes"],
            ],
            &["omlx_phys_footprint_bytes", "phys_footprint_bytes"],
        );
        let resource_limit = json_u64_paths_or_keys(
            source,
            &[
                &["system", "iogpu_wired_limit_bytes"],
                &["system", "metal_limit_bytes"],
                &["mlx", "resource_limit"],
            ],
            &[
                "iogpu_wired_limit_bytes",
                "metal_limit_bytes",
                "resource_limit",
            ],
        );
        let next = MlxTelemetry {
            version: json_string_paths_or_keys(
                source,
                &[
                    &["mlx_version"],
                    &["mlx", "version"],
                    &["engines", "mlx-lm", "version"],
                    &["engines", "mlx-vlm", "version"],
                    &["engines", "mlx-embeddings", "version"],
                    &["engines", "mlx-audio", "version"],
                ],
                &["mlx_version"],
            ),
            active_memory,
            cache_memory,
            peak_memory,
            device_name: json_string_paths_or_keys(
                source,
                &[
                    &["device_name"],
                    &["mlx_device_name"],
                    &["hardware", "device_name"],
                    &["chip_name"],
                ],
                &["device_name", "mlx_device_name", "chip_name"],
            ),
            architecture: json_string_paths_or_keys(
                source,
                &[&["architecture"], &["mlx", "architecture"]],
                &["architecture"],
            ),
            memory_size: json_u64_paths_or_keys(
                source,
                &[
                    &["memory_size"],
                    &["mlx", "memory_size"],
                    &["hardware", "memory_size"],
                    &["system", "total_memory_bytes"],
                ],
                &["memory_size", "total_memory_bytes"],
            )
            .or_else(|| json_f64_key(source, "memory_gb").and_then(gib_to_bytes)),
            recommended_working_set: json_u64_paths_or_keys(
                source,
                &[
                    &["max_recommended_working_set_size"],
                    &["mlx", "max_recommended_working_set_size"],
                    &["recommended_working_set_bytes"],
                ],
                &[
                    "max_recommended_working_set_size",
                    "recommended_working_set_bytes",
                ],
            ),
            max_buffer_size: json_u64_paths_or_keys(
                source,
                &[
                    &["max_buffer_size"],
                    &["mlx", "max_buffer_size"],
                    &["max_buffer_length"],
                ],
                &["max_buffer_size", "max_buffer_length"],
            ),
            resource_limit,
            process_footprint,
        };
        telemetry = merge_mlx_telemetry(&telemetry, &next);
    }
    telemetry
}

fn mlx_metadata_is_empty(telemetry: &MlxTelemetry) -> bool {
    telemetry.version.is_none()
        && telemetry.active_memory.is_none()
        && telemetry.cache_memory.is_none()
        && telemetry.peak_memory.is_none()
        && telemetry.device_name.is_none()
        && telemetry.architecture.is_none()
        && telemetry.memory_size.is_none()
        && telemetry.recommended_working_set.is_none()
        && telemetry.max_buffer_size.is_none()
        && telemetry.resource_limit.is_none()
        && telemetry.process_footprint.is_none()
}

fn merge_mlx_telemetry(base: &MlxTelemetry, update: &MlxTelemetry) -> MlxTelemetry {
    MlxTelemetry {
        version: update.version.clone().or_else(|| base.version.clone()),
        active_memory: update.active_memory.or(base.active_memory),
        cache_memory: update.cache_memory.or(base.cache_memory),
        peak_memory: update.peak_memory.or(base.peak_memory),
        device_name: update
            .device_name
            .clone()
            .or_else(|| base.device_name.clone()),
        architecture: update
            .architecture
            .clone()
            .or_else(|| base.architecture.clone()),
        memory_size: update.memory_size.or(base.memory_size),
        recommended_working_set: update
            .recommended_working_set
            .or(base.recommended_working_set),
        max_buffer_size: update.max_buffer_size.or(base.max_buffer_size),
        resource_limit: update.resource_limit.or(base.resource_limit),
        process_footprint: update.process_footprint.or(base.process_footprint),
    }
}

fn json_u64_paths_or_keys(value: &Value, paths: &[&[&str]], keys: &[&str]) -> Option<u64> {
    paths
        .iter()
        .find_map(|path| json_u64(value, path))
        .or_else(|| keys.iter().find_map(|key| json_u64_key(value, key)))
}

fn json_string_paths_or_keys(value: &Value, paths: &[&[&str]], keys: &[&str]) -> Option<String> {
    paths
        .iter()
        .find_map(|path| json_string(value, path))
        .or_else(|| keys.iter().find_map(|key| json_string_key(value, key)))
}

fn json_u64_key(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(object) => object
            .get(key)
            .and_then(value_as_u64)
            .or_else(|| object.values().find_map(|child| json_u64_key(child, key))),
        Value::Array(values) => values.iter().find_map(|child| json_u64_key(child, key)),
        _ => None,
    }
}

fn json_string_key(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(object) => object
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                object
                    .values()
                    .find_map(|child| json_string_key(child, key))
            }),
        Value::Array(values) => values.iter().find_map(|child| json_string_key(child, key)),
        _ => None,
    }
}

fn json_f64_key(value: &Value, key: &str) -> Option<f64> {
    match value {
        Value::Object(object) => object
            .get(key)
            .and_then(value_as_f64)
            .or_else(|| object.values().find_map(|child| json_f64_key(child, key))),
        Value::Array(values) => values.iter().find_map(|child| json_f64_key(child, key)),
        _ => None,
    }
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_u64().map(|value| value as f64))
}

fn gib_to_bytes(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let bytes = value * 1024_f64.powi(3);
    (bytes.is_finite() && bytes <= u64::MAX as f64).then_some(bytes.round() as u64)
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| value as u64)
    })
}

fn json_string(value: &Value, path: &[&str]) -> Option<String> {
    json_value(value, path)?.as_str().map(str::to_owned)
}

fn json_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let value = json_value(value, path)?;
    value_as_u64(value)
}

fn json_f64(value: &Value, path: &[&str]) -> Option<f64> {
    let value = json_value(value, path)?;
    value
        .as_f64()
        .or_else(|| value.as_u64().map(|value| value as f64))
}

fn request_rate(request: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| json_f64(request, &[*key]))
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn json_value<'a>(mut value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for key in path {
        value = value.get(*key)?;
    }
    Some(value)
}

fn read_llm_stats(home: Option<&Path>) -> LlmLogStats {
    let Some(home) = home else {
        return LlmLogStats::default();
    };
    let candidates = [
        home.join(".omlx-coding/logs/server.log"),
        home.join(".omlx-coding/logs/launchd.stdout.log"),
        home.join(".omlx-coding/logs/launchd.stderr.log"),
    ];
    candidates
        .into_iter()
        .filter_map(|path| read_latest_completion(&path))
        .max_by(|left, right| match (left.observed_at, right.observed_at) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => std::cmp::Ordering::Equal,
        })
        .unwrap_or_default()
}

fn read_latest_completion(path: &Path) -> Option<LlmLogStats> {
    let mut file = File::open(path).ok()?;
    let observed_at = file
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    let mut stats = text
        .lines()
        .filter_map(parse_llm_completion_line)
        .next_back()?;
    stats.observed_at = observed_at;
    Some(stats)
}

fn parse_llm_completion_line(line: &str) -> Option<LlmLogStats> {
    let (marker, marker_start) = if let Some(start) = line.find("Responses API: model=") {
        ("Responses API: model=", start)
    } else {
        let start = line.find("Chat completion: model=")?;
        ("Chat completion: model=", start)
    };
    let start = marker_start + marker.len();
    let (model, rest) = line[start..].split_once(", ")?;
    let (output, rest) = rest.split_once(" tokens in ")?;
    let output_tokens = output.trim().parse().ok()?;
    let (seconds, rest) = rest.split_once("s (")?;
    seconds.trim().parse::<f64>().ok()?;
    let (throughput, rest) = rest.split_once(" tok/s)")?;
    let tokens_per_second = throughput.trim().parse().ok()?;
    let prompt_tokens = rest.find("prompt: ").and_then(|prompt_start| {
        rest[prompt_start + "prompt: ".len()..]
            .split(',')
            .next()?
            .trim()
            .parse()
            .ok()
    });
    Some(LlmLogStats {
        model: Some(model.trim().into()),
        tokens_per_second: Some(tokens_per_second),
        output_tokens: Some(output_tokens),
        prompt_tokens,
        observed_at: None,
    })
}

fn find_number(text: &str, needle: &str) -> Option<u64> {
    let start = text.find(needle)? + needle.len();
    let start = text[start..].find('=')? + start + 1;
    let digits: String = text[start..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn find_named_number(text: &str, key: &str) -> Option<u64> {
    find_named_numbers(text, key).into_iter().next()
}

fn find_named_numbers(text: &str, key: &str) -> Vec<u64> {
    let needle = format!("\"{key}\"");
    let mut values = Vec::new();
    let mut offset = 0;
    while let Some(relative) = text[offset..].find(&needle) {
        let start = offset + relative + needle.len();
        let rest = text[start..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            offset = start;
            continue;
        };
        let digits: String = rest
            .chars()
            .skip_while(|character| character.is_whitespace())
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if let Ok(value) = digits.parse() {
            values.push(value);
        }
        offset = start;
    }
    values
}

fn find_named_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = text[start..].trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn command_text(program: &str, args: &[&str]) -> Option<String> {
    let command = || {
        format!(
            "program={} args={}",
            log_field(program),
            args.iter()
                .map(|arg| log_field(arg))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let mut child = match Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            diagnostics_log(
                "WARN",
                "command_spawn_failed",
                format!("{} error={}", command(), log_field(&error.to_string())),
            );
            return None;
        }
    };
    let Some(mut stdout) = child.stdout.take() else {
        diagnostics_log("WARN", "command_stdout_unavailable", command());
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let command_context = command();
    let reader = thread::spawn(move || {
        let mut output = String::new();
        match stdout.read_to_string(&mut output) {
            Ok(_) => Some(output),
            Err(error) => {
                diagnostics_log(
                    "WARN",
                    "command_read_failed",
                    format!(
                        "{} error={}",
                        command_context,
                        log_field(&error.to_string())
                    ),
                );
                None
            }
        }
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = match reader.join() {
                    Ok(Some(output)) => output,
                    Ok(None) => return None,
                    Err(_) => {
                        diagnostics_log("WARN", "command_reader_panicked", command());
                        return None;
                    }
                };
                if !status.success() {
                    diagnostics_log(
                        "WARN",
                        "command_nonzero_exit",
                        format!("{} code={:?}", command(), status.code()),
                    );
                    return None;
                }
                return Some(output);
            }
            Err(error) => {
                diagnostics_log(
                    "WARN",
                    "command_wait_failed",
                    format!("{} error={}", command(), log_field(&error.to_string())),
                );
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
            Ok(None) if Instant::now() >= deadline => {
                diagnostics_log("WARN", "command_timeout", command());
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn delta(current: u64, previous: u64) -> u64 {
    current.saturating_sub(previous)
}

fn rate_bytes(delta_units: u64, unit_bytes: u64, elapsed: Duration) -> u64 {
    let seconds = elapsed.as_secs_f64().max(0.001);
    let value = (delta_units as f64 * unit_bytes as f64 / seconds).round();
    if value.is_finite() && value > 0.0 {
        value.min(u64::MAX as f64) as u64
    } else {
        0
    }
}

fn signed_rate_bytes(current: u64, previous: u64, elapsed: Duration) -> i64 {
    let seconds = elapsed.as_secs_f64().max(0.001);
    let delta = current as f64 - previous as f64;
    let value = (delta / seconds).round();
    if !value.is_finite() {
        0
    } else if value > i64::MAX as f64 {
        i64::MAX
    } else if value < i64::MIN as f64 {
        i64::MIN
    } else {
        value as i64
    }
}

fn push_history(
    history: &mut VecDeque<ChartPoint>,
    value: Option<u64>,
    metric: ChartMetric,
    limit: usize,
    thresholds: Thresholds,
) {
    push_history_with_tone(
        history,
        value,
        value
            .map(|value| metric.tone(value, thresholds))
            .unwrap_or(Tone::Muted),
        limit,
    );
}

fn push_history_with_tone(
    history: &mut VecDeque<ChartPoint>,
    value: Option<u64>,
    tone: Tone,
    limit: usize,
) {
    history.push_back(ChartPoint {
        tone: if value.is_some() { tone } else { Tone::Muted },
        value,
        observed_at: SystemTime::now(),
    });
    while history.len() > limit {
        history.pop_front();
    }
}

fn parse_unit(value: &str) -> u64 {
    let trimmed = value.trim_matches(',');
    let split = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let number = trimmed[..split].parse::<f64>().unwrap_or(0.0);
    let unit = trimmed[split..].to_ascii_uppercase();
    let multiplier = match unit.as_str() {
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0_f64.powi(2),
        "G" | "GB" => 1024.0_f64.powi(3),
        "T" | "TB" => 1024.0_f64.powi(4),
        _ => 1.0,
    };
    (number * multiplier) as u64
}

fn bytes(value: u64) -> String {
    if value >= 1024_u64.pow(3) {
        format!("{:.1} GiB", value as f64 / 1024_f64.powi(3))
    } else if value >= 1024_u64.pow(2) {
        format!("{:.1} MiB", value as f64 / 1024_f64.powi(2))
    } else if value >= 1024 {
        format!("{:.1} KiB", value as f64 / 1024.0)
    } else {
        format!("{value} B")
    }
}

fn optional_bytes(value: Option<u64>) -> String {
    value.map(bytes).unwrap_or_else(|| "—".into())
}

fn compressed_memory_label(sample: &Sample) -> String {
    if !sample.vm_available {
        return "—".into();
    }
    if sample.compressor == 0 || sample.compressed_logical == 0 {
        return bytes(sample.compressor);
    }
    format!(
        "{} · {:.1}×",
        bytes(sample.compressor),
        sample.compressed_logical as f64 / sample.compressor as f64
    )
}

fn rate(value: u64) -> String {
    format!("{}/s", bytes(value))
}

fn signed_rate(value: i64) -> String {
    if value > 0 {
        format!("+{}", rate(value as u64))
    } else if value < 0 {
        format!("-{}", rate(value.unsigned_abs()))
    } else {
        "0 B/s".into()
    }
}

fn now_clock(host: &dyn Host) -> String {
    if let Some(value) = host.command("/bin/date", &["+%H:%M:%S"]) {
        return value.trim().to_string();
    }
    "??:??:??".into()
}

fn tone_badge(tone: Tone, label: &str) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(Color::Black)
            .bg(tone.color())
            .add_modifier(Modifier::BOLD),
    )
}

fn card_block<'a>(title: Line<'a>, tone: Tone) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(EDGE))
        .style(Style::default().bg(PANEL_RAISED).fg(tone.color()))
}

fn llm_status_tone(status: &str) -> Tone {
    match status.to_ascii_lowercase().as_str() {
        "ready" | "idle" => Tone::Green,
        "waiting" | "busy" | "generating" => Tone::Cyan,
        "stale" => Tone::Yellow,
        "last result" | "offline" => Tone::Muted,
        "error" => Tone::Red,
        _ => Tone::Cyan,
    }
}

fn panel(title: &str, tone: Tone) -> Block<'static> {
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(tone.color())
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .style(Style::default().bg(PANEL))
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// Write the `--once` report. `platform` selects the GPU section, so either
/// platform's layout can be checked from any host.
fn write_static(
    out: &mut dyn Write,
    sample: &Sample,
    interval: u64,
    thresholds: Thresholds,
    platform: Platform,
) -> io::Result<()> {
    writeln!(out, "mlxtop · static report ({interval}s sample)\n")?;
    writeln!(out, "SIGNAL       {} · {}", sample.impact, sample.grade)?;
    writeln!(out, "PRESSURE     {}", pressure_state_label(sample))?;
    writeln!(
        out,
        "MEMORY       {} resident / {} total · includes file cache",
        sample
            .resident_memory
            .map(bytes)
            .unwrap_or_else(|| "—".into()),
        bytes(sample.total_memory)
    )?;
    if sample.swap_available && sample.swap_total == 0 {
        writeln!(
            out,
            "PAGING       SWAP 0 B · not allocated · in {} · out {}",
            rate(sample.swap_in),
            rate(sample.swap_out)
        )?;
    } else if sample.swap_available {
        let used_percent = sample
            .swap_used
            .saturating_mul(100)
            .checked_div(sample.swap_total)
            .unwrap_or(0);
        writeln!(
            out,
            "PAGING       {} / {} · {}% used · in {} · out {}",
            bytes(sample.swap_used),
            bytes(sample.swap_total),
            used_percent,
            rate(sample.swap_in),
            rate(sample.swap_out)
        )?;
    } else {
        writeln!(out, "PAGING       —")?;
    }
    if sample.has_nvidia_gpus() {
        for line in gpu_dashboard::static_lines(&sample.gpus, thresholds) {
            writeln!(out, "{line}")?;
        }
    } else if platform == Platform::MacOs {
        writeln!(
            out,
            "METAL        {} · {} cores · GPU {} · renderer {} · tiler {}",
            sample.metal.device_name.as_deref().unwrap_or("unavailable"),
            sample
                .metal
                .gpu_cores
                .map(|value| value.to_string())
                .unwrap_or_else(|| "—".into()),
            sample
                .gpu_util
                .map(|v| format!("{v}%"))
                .unwrap_or_else(|| "—".into()),
            percent_u8(sample.metal.renderer_util),
            percent_u8(sample.metal.tiler_util)
        )?;
    } else {
        writeln!(out, "GPU          NVIDIA counters unavailable")?;
    }
    writeln!(
        out,
        "RUNTIME      thermal {} · LLM {} · footprint {} · Metal limit {}",
        sample.thermal,
        sample.llm_count,
        optional_bytes(sample.mlx.process_footprint),
        optional_bytes(sample.metal.resource_limit)
    )?;
    writeln!(
        out,
        "LLM          {} · {} · {} · {}",
        sample.llm_provider,
        sample.llm_status,
        telemetry_source(sample),
        llm_model_label(sample, 40)
    )?;
    if let Some(details) = &sample.llm_details {
        writeln!(out, "PROVIDER     {details}")?;
    }
    writeln!(
        out,
        "SERVING      {} · {} · active {} · cache {}",
        llm_generation_rate_label(sample),
        llm_prefill_rate_label(sample),
        sample
            .llm_active_requests
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".into()),
        percent(sample.llm_cache_efficiency)
    )?;
    writeln!(
        out,
        "TOKENS       PROMPT {} · OUT {}",
        optional_tokens(sample.llm_prompt_tokens),
        optional_tokens(sample.llm_output_tokens)
    )?;
    for request in &sample.llm_requests {
        writeln!(out, "REQUEST      {}", request.summary())?;
    }
    if let Some(memory) = &sample.process_memory {
        writeln!(
            out,
            "PROCESS OS   pid {} · footprint {} · lifetime peak {} · RSS {} · growth {}",
            memory.pid,
            bytes(memory.footprint),
            bytes(memory.peak),
            bytes(memory.resident),
            sample
                .process_memory_growth
                .map(signed_rate)
                .unwrap_or_else(|| "—".into())
        )?;
    }
    writeln!(
        out,
        "MLX          version {} · active {} · cache {} · peak {}",
        sample.mlx.version.as_deref().unwrap_or("—"),
        optional_bytes(sample.mlx.active_memory),
        optional_bytes(sample.mlx.cache_memory),
        optional_bytes(sample.mlx.peak_memory)
    )?;
    if !sample.correlation.summary.is_empty() {
        writeln!(out, "CORRELATION   {}", sample.correlation.summary)?;
        writeln!(
            out,
            "EVIDENCE      {} · {} confidence",
            sample.correlation.details,
            sample.correlation.confidence_label()
        )?;
    }
    let finding = diagnosis::assess(sample);
    writeln!(out, "DIAGNOSIS    {}", finding.title)?;
    writeln!(
        out,
        "EVIDENCE     {} · {}",
        finding.evidence, finding.context
    )?;
    writeln!(
        out,
        "{}         {}",
        if finding.actionable { "CHECK" } else { "NOTE " },
        finding.next
    )?;
    Ok(())
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn new() -> Self {
        Self { active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let mut out = stdout();
            let _ = execute!(
                out,
                crossterm::cursor::Show,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        }
    }
}

/// Wait up to `timeout` for one terminal event.
fn next_terminal_event(timeout: Duration) -> io::Result<Option<Event>> {
    match event::poll(timeout) {
        Ok(false) => Ok(None),
        Ok(true) => event::read().map(Some).inspect_err(|error| {
            diagnostics_log(
                "ERROR",
                "input_read_error",
                format!("error={}", log_field(&error.to_string())),
            );
        }),
        Err(error) => {
            diagnostics_log(
                "ERROR",
                "input_poll_error",
                format!("error={}", log_field(&error.to_string())),
            );
            Err(error)
        }
    }
}

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    next_event: &mut dyn FnMut(Duration) -> io::Result<Option<Event>>,
) -> Result<(), Box<dyn std::error::Error>> {
    diagnostics_log("INFO", "tui_start", "interactive_session_started");
    loop {
        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| app.tick())) {
            diagnostics_log(
                "ERROR",
                "tui_tick_panic",
                format!(
                    "tab={} status={} message={}",
                    app.tab,
                    log_field(&app.collector.current.llm_status),
                    log_field(&panic_payload(payload.as_ref()))
                ),
            );
            panic::resume_unwind(payload);
        }
        let terminal_size = terminal
            .size()
            .ok()
            .map(|size| format!("{}x{}", size.width, size.height))
            .unwrap_or_else(|| "unknown".into());
        let app_for_draw = &mut *app;
        let draw_result = panic::catch_unwind(AssertUnwindSafe(|| {
            terminal.draw(move |frame| app_for_draw.draw(frame))
        }));
        let draw_result = match draw_result {
            Ok(result) => result,
            Err(payload) => {
                diagnostics_log(
                    "ERROR",
                    "tui_draw_panic",
                    format!(
                        "tab={} terminal={} status={} message={}",
                        app.tab,
                        terminal_size,
                        log_field(&app.collector.current.llm_status),
                        log_field(&panic_payload(payload.as_ref()))
                    ),
                );
                panic::resume_unwind(payload);
            }
        };
        if let Err(error) = draw_result {
            diagnostics_log(
                "ERROR",
                "terminal_draw_error",
                format!("error={}", log_field(&error.to_string())),
            );
            return Err(error.into());
        }
        if app.quit {
            break;
        }
        match next_event(Duration::from_millis(100))? {
            Some(Event::Key(key)) => app.handle_key(key),
            Some(Event::Mouse(mouse)) => app.handle_mouse(mouse),
            Some(_) | None => {}
        }
    }
    diagnostics_log("INFO", "tui_stop", "interactive_session_stopped");
    Ok(())
}

fn main() {
    let _ = init_diagnostics();
    install_panic_hook();
    match run() {
        Ok(()) => diagnostics_log("INFO", "process_exit", "code=0"),
        Err(error) => {
            diagnostics_log(
                "ERROR",
                "process_error",
                format!("error={}", log_field(&error.to_string())),
            );
            eprintln!("mlxtop: {error}");
            std::process::exit(1);
        }
    }
}

/// What the command line asks for, after config defaults are applied.
#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    /// Print this text (version or help) and exit successfully.
    Print(String),
    Run {
        interval: u64,
        history: usize,
        once: bool,
    },
}

fn help_text(platform: Platform) -> String {
    format!(
        "Usage: mlxtop [refresh-seconds] [options]\n\n\
         Options: -i, --interval N  refresh interval (default 1)\n\
         -n, --history N    chart/journal history (20–3600)\n\
         -1, --once         static report\n\
         -V, --version      show version\n\
         -h, --help         show help\n\
         Config file: ~/.config/mlxtop/config.json\n\
         Diagnostics: {} (override with MLXTOP_LOG_PATH)\n\n\
         Interactive keys: q quit · 1 overview · 2 top · 3 journal · tab views · arrows charts · +/- zoom · enter expand · {{/}} interval · ? help",
        diagnostics_default_hint(platform)
    )
}

fn parse_args(args: &[String], config: &Config) -> Result<CliAction, Box<dyn std::error::Error>> {
    let (mut interval, interval_rejected) = config_interval(config);
    let (mut history, history_rejected) = config_history(config);
    if interval_rejected {
        diagnostics_log(
            "WARN",
            "config_out_of_range",
            format!(
                "field=interval value={:?} allowed={INTERVAL_MIN}..={INTERVAL_MAX} using={interval}",
                config.interval
            ),
        );
    }
    if history_rejected {
        diagnostics_log(
            "WARN",
            "config_out_of_range",
            format!(
                "field=history value={:?} allowed={HISTORY_MIN}..={HISTORY_MAX} using={history}",
                config.history
            ),
        );
    }
    let mut once = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--interval" => {
                i += 1;
                interval = args.get(i).ok_or("missing interval")?.parse()?;
            }
            "-n" | "--history" => {
                i += 1;
                history = args.get(i).ok_or("missing history")?.parse()?;
            }
            "-1" | "--once" => once = true,
            "-V" | "--version" => return Ok(CliAction::Print(format!("mlxtop {VERSION}"))),
            "-h" | "--help" => return Ok(CliAction::Print(help_text(Platform::current()))),
            value if !value.starts_with('-') && i == 0 => interval = value.parse()?,
            value => return Err(format!("unknown option: {value}").into()),
        }
        i += 1;
    }
    if !(INTERVAL_MIN..=INTERVAL_MAX).contains(&interval) {
        return Err("interval must be between 1 and 60 seconds".into());
    }
    if !(HISTORY_MIN..=HISTORY_MAX).contains(&history) {
        return Err("history must be between 20 and 3600".into());
    }
    Ok(CliAction::Run {
        interval,
        history,
        once,
    })
}

/// Take two samples `interval` apart, so rates are measured, and report.
fn run_once(collector: &mut Collector, interval: Duration, out: &mut dyn Write) -> io::Result<()> {
    collector.sample();
    thread::sleep(interval);
    let sample = collector.sample();
    write_static(
        out,
        &sample,
        interval.as_secs(),
        collector.thresholds,
        collector.platform,
    )
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = load_config();
    let args: Vec<String> = env::args().skip(1).collect();
    let (interval, history, once) = match parse_args(&args, &config)? {
        CliAction::Print(text) => {
            println!("{text}");
            return Ok(());
        }
        CliAction::Run {
            interval,
            history,
            once,
        } => (interval, history, once),
    };

    diagnostics_log(
        "INFO",
        "configuration",
        format!(
            "interval_seconds={interval} history_limit={history} once={once} interactive={} config_path={}",
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            config_path().display()
        ),
    );

    if once {
        let mut collector = Collector::new(history, config);
        run_once(
            &mut collector,
            Duration::from_secs(interval),
            &mut stdout().lock(),
        )?;
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("run interactively, or use --once for a static report".into());
    }

    enable_raw_mode()?;
    let mut terminal_guard = TerminalGuard::new();
    let mut out = stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(interval, history, config);
    let result = run_app(&mut terminal, &mut app, &mut next_terminal_event);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::cursor::Show,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    terminal_guard.disarm();
    drop(app);
    result
}

#[cfg(test)]
#[path = "tests/app.rs"]
mod app_tests;
#[cfg(test)]
#[path = "tests/collector.rs"]
mod collector_tests;
#[cfg(test)]
#[path = "tests/support.rs"]
mod test_support;
#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;
