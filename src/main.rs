//! PcieWatch — tray monitor for GPU PCIe link degradation.
//!
//! Alarm signals (power-management aware):
//!   * width < baseline sustained (debounced)  -> DEGRADED (the sagging signature;
//!     width is not affected by power management, so no false positives)
//!   * gen < baseline only while GPU is active -> WARN (idle speed dips are power
//!     management and are deliberately not toasted)
//!   * poller errors / no samples at all       -> LOST (GPU off the bus or driver hang)
//!
//! The main thread runs a Win32 message pump (required by tray-icon on Windows).
//! A worker thread polls NVML in-process.

#![windows_subsystem = "windows"]

mod poller;
mod win32;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder};

const APP_NAME: &str = "PcieWatch";
/// Poll intervals offered in the tray menu (seconds). The LOST watchdog is
/// "3 missed cycles", which scales with the chosen interval (15 s at 5 s).
const POLL_SECS: [u64; 5] = [5, 10, 30, 60, 300];

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
struct Config {
    gpu_index: u32,
    baseline_gen: u32,
    baseline_width: u32,
    poll_ms: u64,
    width_debounce: u32,
    util_active_threshold: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gpu_index: 0,
            // Gen3 x16: conservative default — only links *worse* than the baseline
            // ever alarm, so it is safe on Gen4/Gen5 platforms too; "Re-learn
            // baseline" in the tray menu updates it to match the actual link.
            baseline_gen: 3,
            baseline_width: 16,
            poll_ms: 5000,
            width_debounce: 3,
            util_active_threshold: 20,
        }
    }
}

fn data_dir() -> std::path::PathBuf {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    let dir = std::path::Path::new(&local).join(APP_NAME);
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn load_config() -> Config {
    // First run: no file yet, nothing to complain about.
    let Some(s) = std::fs::read_to_string(data_dir().join("config.json")).ok() else {
        return Config::default();
    };
    let Ok(mut cfg) = serde_json::from_str::<Config>(&s) else {
        log_line("config.json unparseable — using defaults");
        return Config::default();
    };
    // Clamp hand-edited values into sane ranges (a 0/absurd poll_ms would
    // busy-loop the poller or disable the watchdog).
    if cfg.poll_ms < 1_000 || cfg.poll_ms > 3_600_000 {
        log_line(&format!(
            "config: poll_ms {} out of range — clamped to {}",
            cfg.poll_ms,
            cfg.poll_ms.clamp(1_000, 3_600_000)
        ));
        cfg.poll_ms = cfg.poll_ms.clamp(1_000, 3_600_000);
    }
    if cfg.width_debounce < 1 || cfg.width_debounce > 10 {
        let c = cfg.width_debounce.clamp(1, 10);
        log_line(&format!(
            "config: width_debounce {} out of range — clamped to {c}",
            cfg.width_debounce
        ));
        cfg.width_debounce = c;
    }
    if cfg.util_active_threshold > 100 {
        log_line(&format!(
            "config: util_active_threshold {} out of range — clamped to 100",
            cfg.util_active_threshold
        ));
        cfg.util_active_threshold = 100;
    }
    cfg
}

fn save_config(cfg: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        std::fs::write(data_dir().join("config.json"), s).ok();
    }
}

fn log_line(msg: &str) {
    // ISO 8601 UTC on every line (ms resolution), and the daily file rotation
    // is keyed on the UTC date so filename and line timestamps always agree.
    let now = chrono::Utc::now();
    let path = data_dir().join(format!("pciewatch-{}.log", now.format("%Y%m%d")));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}Z {}", now.format("%Y-%m-%dT%H:%M:%S%.3f"), msg);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    Normal,
    Warn,
    Degraded,
    Lost,
}

impl Level {
    fn color(self) -> (u8, u8, u8) {
        match self {
            Level::Normal => (86, 198, 108),
            Level::Warn => (255, 183, 3),
            Level::Degraded => (255, 138, 0),
            Level::Lost => (239, 65, 58),
        }
    }
}

fn icon(level: Level) -> Icon {
    let (r, g, b) = level.color();
    // 32x32: the tray down-scales crisply; a 16x16 source blurs up on
    // high-DPI taskbars.
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let c = 15.0f32;
    let rad = 9.0f32;
    for y in 0..size {
        for x in 0..size {
            let ax = (x as f32 - c).abs();
            let ay = (y as f32 - c).abs();
            let mut inside = ax <= c && ay <= c;
            if ax > c - rad && ay > c - rad {
                let dx = ax - (c - rad);
                let dy = ay - (c - rad);
                inside = dx * dx + dy * dy <= rad * rad;
            }
            if inside {
                let i = ((y * size + x) * 4) as usize;
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, size, size).expect("icon")
}

fn pstate_name(p: &nvml_wrapper::enum_wrappers::device::PerformanceState) -> &'static str {
    use nvml_wrapper::enum_wrappers::device::PerformanceState::*;
    match p {
        Zero => "P0",
        One => "P1",
        Two => "P2",
        Three => "P3",
        Four => "P4",
        Five => "P5",
        Six => "P6",
        Seven => "P7",
        Eight => "P8",
        Nine => "P9",
        Ten => "P10",
        Eleven => "P11",
        _ => "P?",
    }
}

fn autostart_enabled() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run") {
        Ok(k) => k.get_value::<String, _>(APP_NAME).is_ok(),
        Err(_) => false,
    }
}

fn set_autostart(on: bool) -> Result<(), String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    let run = hkcu
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            KEY_READ | KEY_WRITE,
        )
        .map_err(|e| e.to_string())?;
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        run.set_value(APP_NAME, &format!("\"{}\"", exe.to_string_lossy()))
            .map_err(|e| e.to_string())?;
    } else {
        run.delete_value(APP_NAME).map_err(|e| e.to_string())?;
    }
    Ok(())
}

struct Ui {
    level: Level,
    last_sample: Option<poller::Sample>,
    consec_err: u32,
    last_event: Instant,
    status: MenuItem,
    autostart: MenuItem,
    poll_items: Vec<CheckMenuItem>,
    poll_ms: Arc<AtomicU64>,
    poll_cmd: mpsc::Sender<poller::Command>,
    cfg: Config,
}

fn status_text(ui: &Ui, s: &poller::Sample) -> String {
    format!(
        "Gen{} x{}  (want Gen{} x{})   {}% util  {}",
        s.gen,
        s.width,
        ui.cfg.baseline_gen,
        ui.cfg.baseline_width,
        s.util,
        pstate_name(&s.pstate)
    )
}

/// Refresh the tray for a new level. The icon and balloons act on transitions
/// only; the tooltip always reflects the latest sample — otherwise the first
/// sample at the initial level (Normal) would leave it stuck on "starting…".
fn set_level(ui: &mut Ui, tray: &tray_icon::TrayIcon, level: Level, s: Option<&poller::Sample>) {
    let changed = level != ui.level;
    if changed {
        let prev = ui.level;
        ui.level = level;
        tray.set_icon(Some(icon(level))).ok();

        match (prev, level, s) {
            (_, Level::Degraded, Some(s)) => {
                log_line(&format!(
                    "toast: PCIe link degraded (Gen{} x{})",
                    s.gen, s.width
                ));
                win32::balloon(
                    "PCIe link degraded",
                    format!(
                        "Link is now Gen{} x{} (baseline Gen{} x{}). Possible loose GPU — reseat the card.",
                        s.gen, s.width, ui.cfg.baseline_gen, ui.cfg.baseline_width
                    ),
                    win32::Kind::Error,
                )
            }
            (_, Level::Lost, _) => {
                log_line("toast: GPU not responding (no samples)");
                win32::balloon(
                    "GPU not responding",
                    "No PCIe link samples — check the slot, the card, and the driver.",
                    win32::Kind::Error,
                )
            }
            (Level::Degraded | Level::Lost, Level::Normal, Some(s)) => {
                log_line(&format!(
                    "toast: PCIe link restored (Gen{} x{})",
                    s.gen, s.width
                ));
                win32::balloon(
                    "PCIe link restored",
                    format!("Back to Gen{} x{} — link is healthy again.", s.gen, s.width),
                    win32::Kind::Info,
                )
            }
            _ => {} // Normal<->Warn: icon/tooltip only, no toast
        }
    }

    // Cheap NIM_MODIFY (same string is a no-op visually); keeps the hover text
    // current on every sample, not just on level changes. Goes through the raw
    // GUID-stamped setter because tray-icon 0.25's set_tooltip skips NIF_GUID.
    let tip = match (level, s) {
        (Level::Lost, _) => "PCIe Watch — GPU not responding".to_string(),
        (Level::Degraded, Some(s)) => format!("PCIe Watch — DEGRADED Gen{} x{}", s.gen, s.width),
        (_, Some(s)) => format!("PCIe Watch — Gen{} x{}", s.gen, s.width),
        _ => "PCIe Watch".to_string(),
    };
    win32::set_tip(tray.window_handle() as _, &tip);
}

fn mid(s: &str) -> tray_icon::menu::MenuId {
    s.parse().expect("menu id")
}

/// Opens the log/config folder from the tray menu.
fn open_log_dir() {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).ok();
    if let Err(e) = std::process::Command::new("explorer")
        .arg(dir.to_string_lossy().as_ref())
        .spawn()
    {
        log_line(&format!("could not open log folder: {e}"));
    }
}

fn main() {
    if !win32::ensure_singleton() {
        log_line("another instance is already running — exiting");
        std::process::exit(0);
    }

    let cfg = load_config();
    log_line(&format!(
        "=== PcieWatch starting (baseline Gen{} x{}, poll {} ms) ===",
        cfg.baseline_gen, cfg.baseline_width, cfg.poll_ms
    ));

    let status = MenuItem::with_id(mid("status"), "Status: initializing…", false, None);
    let test = MenuItem::with_id(mid("test"), "Send test notification", true, None);
    let relearn = MenuItem::with_id(
        mid("relearn"),
        "Re-learn baseline from current link",
        true,
        None,
    );
    let autostart = MenuItem::with_id(
        mid("autostart"),
        format!(
            "Launch at login: {}",
            if autostart_enabled() { "on" } else { "off" }
        ),
        true,
        None,
    );
    let exit = MenuItem::with_id(mid("exit"), "Exit PcieWatch", true, None);

    // "Poll every" submenu: one check item per interval.
    let poll_items: Vec<CheckMenuItem> = POLL_SECS
        .iter()
        .map(|s| {
            CheckMenuItem::with_id(
                mid(format!("poll_{s}").as_str()),
                format!("{s}s"),
                true,
                *s * 1000 == cfg.poll_ms,
                None,
            )
        })
        .collect();
    let poll_header = Submenu::with_id(mid("poll"), "Poll every", true);
    for item in &poll_items {
        poll_header.append(item).ok();
    }
    let open_logs = MenuItem::with_id(mid("logs"), "Open log folder", true, None);

    let menu = Menu::new();
    let _ = menu.append(&status);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&test);
    let _ = menu.append(&relearn);
    let _ = menu.append(&poll_header);
    let _ = menu.append(&open_logs);
    let _ = menu.append(&autostart);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit);

    // Must be created on a thread with a running message pump (see win32::pump).
    // with_guid lets win32::balloon target this exact icon via NIM_MODIFY|NIF_GUID.
    let tray = TrayIconBuilder::new()
        .with_id(APP_NAME.parse::<tray_icon::TrayIconId>().unwrap())
        .with_guid(win32::TRAY_GUID)
        .with_icon(icon(Level::Normal))
        .with_tooltip("PCIe Watch — starting…")
        .with_menu(Box::new(menu))
        .build()
        .unwrap_or_else(|e| panic!("tray init failed: {e}"));

    // Shared with the poller: menu clicks store a new interval there, the
    // poller re-reads it every 500 ms while sleeping.
    let poll_ms = Arc::new(AtomicU64::new(cfg.poll_ms));
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel::<poller::Command>();

    let (tx, rx) = mpsc::channel::<poller::UiEvent>();
    poller::spawn(cfg.clone(), tx, poll_ms.clone(), poll_cmd_rx);
    let menu_rx = MenuEvent::receiver();

    let mut ui = Ui {
        level: Level::Normal,
        last_sample: None,
        consec_err: 0,
        last_event: Instant::now(),
        status,
        autostart,
        poll_items,
        poll_ms,
        poll_cmd: poll_cmd_tx,
        cfg,
    };

    win32::pump(move || {
        // --- drain poller events ---
        while let Ok(ev) = rx.try_recv() {
            ui.last_event = Instant::now();
            match ev {
                poller::UiEvent::Sample(s, level) => {
                    ui.consec_err = 0;
                    let text = status_text(&ui, &s);
                    ui.status.set_text(text.as_str());
                    ui.last_sample = Some(s.clone());
                    set_level(&mut ui, &tray, level, Some(&s));
                }
                poller::UiEvent::Error(e) => {
                    ui.consec_err = ui.consec_err.saturating_add(1);
                    log_line(&format!("poller error: {e}"));
                    ui.status.set_text("Status: poller error");
                    if ui.consec_err >= 2 {
                        set_level(&mut ui, &tray, Level::Lost, None);
                        ui.consec_err = 1; // set_level is a no-op while already Lost
                    }
                }
            }
        }

        // --- drain menu clicks ---
        while let Ok(ev) = menu_rx.try_recv() {
            match ev.id().0.as_str() {
                "test" => win32::balloon(
                    "PCIe Watch",
                    "Test notification — if you saw this, the notifier works.",
                    win32::Kind::Info,
                ),
                "relearn" => match ui.last_sample.clone() {
                    Some(s) if s.width == ui.cfg.baseline_width => {
                        ui.cfg.baseline_gen = s.gen;
                        ui.cfg.baseline_width = s.width;
                        save_config(&ui.cfg);
                        log_line(&format!("baseline re-learned to Gen{} x{}", s.gen, s.width));
                        // The poller keeps its own Config copy — push the new
                        // baseline to it so the state machine switches over
                        // within ~0.5 s (applied on its next 500 ms wake).
                        let _ = ui.poll_cmd.send(poller::Command::SetBaseline {
                            gen: s.gen,
                            width: s.width,
                        });
                        win32::balloon(
                            "PCIe Watch",
                            format!("Baseline set to Gen{} x{}.", s.gen, s.width),
                            win32::Kind::Info,
                        );
                        ui.status.set_text(status_text(&ui, &s).as_str());
                    }
                    Some(s) => win32::balloon(
                        "PCIe Watch",
                        format!(
                            "Link width is degraded (x{}). Re-learn refused — fix the link first.",
                            s.width
                        ),
                        win32::Kind::Warning,
                    ),
                    None => win32::balloon(
                        "PCIe Watch",
                        "No samples yet — cannot re-learn.",
                        win32::Kind::Warning,
                    ),
                },
                "poll_5" | "poll_10" | "poll_30" | "poll_60" | "poll_300" => {
                    if let Some(secs) = POLL_SECS
                        .iter()
                        .find(|s| ev.id().0.as_str() == format!("poll_{s}"))
                        .copied()
                    {
                        let ms = secs * 1000;
                        ui.poll_ms.store(ms, Ordering::Relaxed);
                        ui.cfg.poll_ms = ms;
                        save_config(&ui.cfg);
                        log_line(&format!("poll interval -> {secs}s"));
                        for (s, item) in POLL_SECS.iter().zip(ui.poll_items.iter()) {
                            item.set_checked(*s == secs);
                        }
                    }
                }
                "logs" => {
                    log_line("opening log folder (menu)");
                    open_log_dir();
                }
                "autostart" => {
                    let target = !autostart_enabled();
                    match set_autostart(target) {
                        Ok(()) => {
                            log_line(&format!(
                                "autostart -> {}",
                                if target { "on" } else { "off" }
                            ));
                            ui.autostart.set_text(
                                format!("Launch at login: {}", if target { "on" } else { "off" })
                                    .as_str(),
                            );
                        }
                        Err(e) => win32::balloon(
                            "PCIe Watch",
                            format!("Could not change autostart: {e}"),
                            win32::Kind::Warning,
                        ),
                    }
                }
                "exit" => {
                    log_line("=== PcieWatch exiting (menu) ===");
                    // Breaking out of the pump lets main() return, so the
                    // TrayIcon's Drop issues the GUID NIM_DELETE cleanly.
                    return false;
                }
                _ => {}
            }
        }

        // --- staleness watchdog (hard hang: worker silent, no events at all) ---
        // 3 missed cycles; scales with the poll interval so a 5-minute poll
        // doesn't false-positive LOST.
        let stale_after = Duration::from_millis((ui.cfg.poll_ms * 3).max(15_000));
        if ui.last_event.elapsed() > stale_after && ui.level != Level::Lost {
            set_level(&mut ui, &tray, Level::Lost, None);
        }

        true
    });
}
