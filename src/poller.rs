//! NVML poller + link state machine.
//!
//! Runs on a worker thread; queries the GPU in-process via nvml.dll (no subprocess).
//! Sends (sample, level) to the UI thread via mpsc.
//!
//! Level logic (power-management aware):
//!   width < baseline, debounced (3 consecutive polls) => Degraded  (real lane loss)
//!   width < baseline, not yet debounced                => Warn     (amber, no toast)
//!   gen  < baseline while GPU active (util >= thresh)  => Warn     (speed drop under load)
//!   gen  < baseline while GPU idle                     => Normal   (power mgmt, log only)
//!   NVML error                                         => Err to UI (x2 => LOST in UI)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nvml_wrapper::Nvml;

#[derive(Clone)]
pub struct Sample {
    pub gen: u32,
    pub width: u32,
    pub util: u32,
    pub pstate: nvml_wrapper::enum_wrappers::device::PerformanceState,
}

pub enum UiEvent {
    Sample(Sample, crate::Level),
    Error(String),
}

/// Main-thread -> poller commands (drained on the poller's 500 ms wake).
pub enum Command {
    /// Re-learned baseline from the tray menu; the state machine picks it up
    /// from the next sample without a restart.
    SetBaseline { gen: u32, width: u32 },
}

/// Pure level computation, split out of the poll loop so it is unit-testable.
/// Returns the new level and the updated low-width debounce counter.
pub fn next_level(cfg: &crate::Config, s: &Sample, bad_width: u32) -> (crate::Level, u32) {
    if s.width < cfg.baseline_width {
        let bad_width = bad_width.saturating_add(1);
        let level = if bad_width >= cfg.width_debounce {
            crate::Level::Degraded
        } else {
            crate::Level::Warn
        };
        (level, bad_width)
    } else {
        let level = if s.gen < cfg.baseline_gen && s.util >= cfg.util_active_threshold {
            crate::Level::Warn
        } else {
            crate::Level::Normal
        };
        (level, 0)
    }
}

fn drain_commands(cmd_rx: &Receiver<Command>, cfg: &mut crate::Config) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            Command::SetBaseline { gen, width } => {
                if (gen, width) != (cfg.baseline_gen, cfg.baseline_width) {
                    log(&format!("baseline updated to Gen{gen} x{width}"));
                    cfg.baseline_gen = gen;
                    cfg.baseline_width = width;
                }
            }
        }
    }
}

fn sample(nvml: &Nvml, index: u32) -> Result<Sample, String> {
    let dev = nvml
        .device_by_index(index)
        .map_err(|e| format!("gpu {index} not found: {e}"))?;
    let gen = dev.current_pcie_link_gen().map_err(|e| e.to_string())?;
    let width = dev.current_pcie_link_width().map_err(|e| e.to_string())?;
    let util = dev.utilization_rates().map_err(|e| e.to_string())?.gpu;
    let pstate = dev.performance_state().map_err(|e| e.to_string())?;
    Ok(Sample {
        gen,
        width,
        util,
        pstate,
    })
}

fn log(msg: &str) {
    crate::log_line(msg);
}

/// `poll_ms` is hot-swappable from the tray menu ("Poll every"): the loop
/// re-reads it on every 500 ms wake, so a change applies within half a second.
pub fn spawn(
    cfg: crate::Config,
    tx: Sender<UiEvent>,
    poll_ms: Arc<AtomicU64>,
    cmd_rx: Receiver<Command>,
) {
    let _ = std::thread::Builder::new()
        .name("pciewatch-poller".to_string())
        .spawn(move || {
            let nvml = match Nvml::init() {
                Ok(n) => n,
                Err(e) => {
                    log(&format!("NVML init failed: {e}"));
                    let _ = tx.send(UiEvent::Error(format!("NVML init failed: {e}")));
                    return;
                }
            };
            let name = match nvml.device_by_index(cfg.gpu_index).and_then(|d| d.name()) {
                Ok(n) => n,
                Err(_) => String::from("?"),
            };
            log(&format!("poller started for {name}"));

            let mut bad_width = 0u32;
            let mut cfg = cfg;
            let mut last_level: Option<crate::Level> = None;

            loop {
                let tick = Instant::now();
                match sample(&nvml, cfg.gpu_index) {
                    Ok(s) => {
                        let (level, bw) = next_level(&cfg, &s, bad_width);
                        bad_width = bw;
                        // Event log: only level changes are written, so a
                        // healthy box produces essentially no log volume.
                        if last_level != Some(level) {
                            let prev = last_level
                                .map(|l| format!("{l:?}"))
                                .unwrap_or_else(|| "start".to_string());
                            log(&format!(
                                "level change: {prev} -> {level:?} (gen={} width={} util={}% pstate={:?})",
                                s.gen, s.width, s.util, s.pstate
                            ));
                        }
                        last_level = Some(level);
                        let _ = tx.send(UiEvent::Sample(s, level));
                    }
                    Err(e) => {
                        log(&format!("NVML error: {e}"));
                        let _ = tx.send(UiEvent::Error(e));
                    }
                }
                // Sleep in 500 ms chunks: the target interval is re-read and
                // pending commands drained on each wake, so a menu change
                // (poll rate or re-learned baseline) takes effect within ~0.5 s
                // instead of at the end of the current (possibly 5-minute) cycle.
                while tick.elapsed() < Duration::from_millis(poll_ms.load(Ordering::Relaxed)) {
                    drain_commands(&cmd_rx, &mut cfg);
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvml_wrapper::enum_wrappers::device::PerformanceState;

    fn sample(gen: u32, width: u32, util: u32) -> Sample {
        Sample {
            gen,
            width,
            util,
            pstate: PerformanceState::Zero,
        }
    }

    fn cfg() -> crate::Config {
        // Defaults: baseline Gen3 x16, debounce 3, util threshold 20
        crate::Config::default()
    }

    #[test]
    fn full_link_idle_is_normal() {
        assert_eq!(
            next_level(&cfg(), &sample(3, 16, 0), 0),
            (crate::Level::Normal, 0)
        );
    }

    #[test]
    fn speed_drop_under_load_is_warn() {
        assert_eq!(
            next_level(&cfg(), &sample(2, 16, 50), 0),
            (crate::Level::Warn, 0)
        );
    }

    #[test]
    fn speed_drop_while_idle_is_normal() {
        // Power-management artifact: gen dips at idle are not alarmed.
        assert_eq!(
            next_level(&cfg(), &sample(2, 16, 10), 0),
            (crate::Level::Normal, 0)
        );
    }

    #[test]
    fn width_drop_debounces_to_degraded() {
        let (l1, b1) = next_level(&cfg(), &sample(3, 4, 0), 0);
        let (l2, b2) = next_level(&cfg(), &sample(3, 4, 0), b1);
        let (l3, b3) = next_level(&cfg(), &sample(3, 4, 0), b2);
        assert_eq!(
            (l1, l2, l3),
            (
                crate::Level::Warn,
                crate::Level::Warn,
                crate::Level::Degraded
            )
        );
        assert_eq!(b3, 3);
    }

    #[test]
    fn width_recovery_resets_debounce() {
        let (_, b) = next_level(&cfg(), &sample(3, 4, 0), 2); // 3rd low sample -> Degraded
        let (l, b2) = next_level(&cfg(), &sample(3, 16, 0), b);
        assert_eq!((l, b2), (crate::Level::Normal, 0));
        // Next width drop starts the debounce count over from one.
        assert_eq!(
            next_level(&cfg(), &sample(3, 4, 0), b2),
            (crate::Level::Warn, 1)
        );
    }

    #[test]
    fn exact_baseline_gen_while_active_is_normal() {
        // gen == baseline is healthy; only gen < baseline while busy warns.
        assert_eq!(
            next_level(&cfg(), &sample(3, 16, 99), 0),
            (crate::Level::Normal, 0)
        );
    }
}
