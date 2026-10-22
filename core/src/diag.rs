//! Diagnostic instrumentation for the temporary field-test builds (`--features diag`).
//!
//! The wait logic is untouched: this module only records what happened and writes one summary
//! line per second to `fps1000.log`, plus every calibration probe. It exists to answer three
//! questions from a machine we cannot access:
//!
//! 1. which wait length was chosen, and how its real duration drifts afterwards;
//! 2. what the frame period distribution looks like (p50/p90/p99/min/max);
//! 3. whether the simulation is running faster than wall-clock time, which is what makes events
//!    (kill announcements) arrive before the action that caused them.

use core::ffi::c_void;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering};

use windows::core::PCSTR;
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessInformation};

use crate::{log, now_hns, HNS_PER_MS, REQ_HNS};

/// Ring buffer length for the per-frame samples (one summary line per second).
const RING: usize = 512;

static PERIODS: [AtomicI64; RING] = [const { AtomicI64::new(0) }; RING];
static WAITS: [AtomicI64; RING] = [const { AtomicI64::new(0) }; RING];
static SLOT: AtomicUsize = AtomicUsize::new(0);

static FRAMES: AtomicUsize = AtomicUsize::new(0);
static LAST_WAIT: AtomicI64 = AtomicI64::new(0);
static LAST_REPORT: AtomicI64 = AtomicI64::new(0);
static START: AtomicI64 = AtomicI64::new(0);

static N_FLAT: AtomicUsize = AtomicUsize::new(0);
static N_CHUNK: AtomicUsize = AtomicUsize::new(0);
static N_SLEEP: AtomicUsize = AtomicUsize::new(0);
static N_EARLY: AtomicUsize = AtomicUsize::new(0);
static N_TOPOUT: AtomicUsize = AtomicUsize::new(0);
static N_SPIN: AtomicUsize = AtomicUsize::new(0);
static SPIN_SUM: AtomicI64 = AtomicI64::new(0);

static N_ERR: AtomicUsize = AtomicUsize::new(0);
static LAST_ERR: AtomicU32 = AtomicU32::new(0);

static ENG_HOSTF: AtomicU32 = AtomicU32::new(0);
static ENG_SIMF: AtomicU32 = AtomicU32::new(0);

/// Call once from `fps1000_install`.
pub fn on_install() {
    let now = unsafe { now_hns() }.unwrap_or(0);
    START.store(now, Ordering::Relaxed);
    LAST_REPORT.store(now, Ordering::Relaxed);

    log(&format!(
        "diag: build={} plapi={} pid={} qpc_freq={}",
        env!("CARGO_PKG_VERSION"),
        crate::mm::PLAPI_VERSION,
        std::process::id(),
        crate::qpc_freq().unwrap_or(0),
    ));
    log(&format!(
        "diag: timer resolution at install: {}",
        match timer_resolution() {
            Some((now_us, min_us, max_us)) =>
                format!("now={}.{:03}ms min={}.{:03}ms max={}.{:03}ms", now_us / 1000, now_us % 1000, min_us / 1000, min_us % 1000, max_us / 1000, max_us % 1000),
            None => "unavailable".to_string(),
        }
    ));
    log(&format!(
        "diag: power throttling at install: ignore_timer_resolution={}",
        match throttled() {
            Some(true) => "1",
            Some(false) => "0",
            None => "unknown",
        }
    ));
    if let Some((hostf, simf, ft)) = engine_stats() {
        log(&format!(
            "diag: engine at install: hostFrames={} simFrames={} host_frametime={}us",
            hostf,
            simf,
            (ft * 1_000_000.0) as i64
        ));
    }
}

/// Called for every calibration probe.
pub fn on_probe(cand: i64, mean: i64, lo: i64, hi: i64, verdict: &str) {
    log(&format!(
        "diag: probe req={}us mean={}.{:03}ms lo={}.{:03}ms hi={}.{:03}ms spread={}us {}",
        cand / 10,
        mean / HNS_PER_MS,
        (mean % HNS_PER_MS) / 10,
        lo / HNS_PER_MS,
        (lo % HNS_PER_MS) / 10,
        hi / HNS_PER_MS,
        (hi % HNS_PER_MS) / 10,
        (hi - lo) / 10,
        verdict,
    ));
}

/// Called once, when calibration has picked a request.
pub fn on_calibrated(req: i64, kind: &str) {
    let t = elapsed_ms();
    log(&format!(
        "diag: calibrated req={}us ({}) after {}.{:03}s, throttled={}",
        req / 10,
        kind,
        t / 1000,
        t % 1000,
        match throttled() {
            Some(true) => "1",
            Some(false) => "0",
            None => "unknown",
        }
    ));
}

/// Count the path a `Sleep(1)` call took.
pub fn note_path(path: &str) {
    match path {
        "flat" => N_FLAT.fetch_add(1, Ordering::Relaxed),
        "chunk" => N_CHUNK.fetch_add(1, Ordering::Relaxed),
        "sleep" => N_SLEEP.fetch_add(1, Ordering::Relaxed),
        "early" => N_EARLY.fetch_add(1, Ordering::Relaxed),
        "spin" => N_SPIN.fetch_add(1, Ordering::Relaxed),
        _ => N_TOPOUT.fetch_add(1, Ordering::Relaxed),
    };
}

/// Record how long the busy-wait tail of a frame took.
pub fn set_spin(hns: i64) {
    SPIN_SUM.fetch_add(hns, Ordering::Relaxed);
}

/// Record the real duration of the last wait.
pub fn set_wait(us: i64) {
    LAST_WAIT.store(us, Ordering::Relaxed);
}

/// Record a Windows error that would otherwise be swallowed (timer creation, arming, waiting).
///
/// The first few are logged immediately; after that they are only counted, so a host whose timer
/// is permanently broken does not fill the file with the same line every frame.
pub fn note_error(tag: &str, code: u32) {
    let n = N_ERR.fetch_add(1, Ordering::Relaxed);
    LAST_ERR.store(code, Ordering::Relaxed);
    if n < 8 {
        log(&format!("diag: error {tag} code=0x{code:08X}"));
    }
}

/// Record one frame period; emits the summary line when a second has passed.
pub fn record(period: i64) {
    let i = SLOT.fetch_add(1, Ordering::Relaxed) % RING;
    PERIODS[i].store(period, Ordering::Relaxed);
    WAITS[i].store(LAST_WAIT.load(Ordering::Relaxed), Ordering::Relaxed);
    FRAMES.fetch_add(1, Ordering::Relaxed);

    let Some(now) = (unsafe { now_hns() }) else { return };
    let last = LAST_REPORT.load(Ordering::Relaxed);
    if now - last < 10_000_000 {
        return;
    }
    if LAST_REPORT
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return; // another thread is reporting
    }
    report(now);
}

fn report(now: i64) {
    let start = START.load(Ordering::Relaxed);
    let t_ms = if start > 0 { (now - start) / 10_000 } else { 0 };

    let mut periods: Vec<i64> = PERIODS.iter().map(|a| a.load(Ordering::Relaxed)).filter(|v| *v > 0).collect();
    let mut waits: Vec<i64> = WAITS.iter().map(|a| a.load(Ordering::Relaxed)).filter(|v| *v > 0).collect();
    if periods.is_empty() {
        return;
    }
    periods.sort_unstable();
    waits.sort_unstable();

    let pct = |v: &[i64], p: usize| -> i64 {
        if v.is_empty() {
            0
        } else {
            v[(v.len() - 1) * p / 100]
        }
    };
    let mean = periods.iter().sum::<i64>() / periods.len() as i64;

    let req = REQ_HNS.load(Ordering::Relaxed);
    let req_text = if req <= 0 {
        "chunked".to_string()
    } else {
        format!("{}us", req / 10)
    };

    let n_spin = N_SPIN.load(Ordering::Relaxed);
    let spin_sum = SPIN_SUM.load(Ordering::Relaxed);
    let spin_avg = if n_spin > 0 {
        hns_us(spin_sum / n_spin as i64)
    } else {
        0.0
    };

    // Frame-period histogram over the ring: the `900-1100` bucket is the healthy one, and any
    // sample below 900 us is a frame the engine would clamp and credit with a full millisecond.
    let mut hist = [0usize; 6];
    for p in &periods {
        let us = p / 10;
        let i = if us < 600 {
            0
        } else if us < 900 {
            1
        } else if us < 1100 {
            2
        } else if us < 1500 {
            3
        } else if us < 2500 {
            4
        } else {
            5
        };
        hist[i] += 1;
    }

    let mut line = format!(
        "diag: t={}.{:03}s frames={} fps={} mean={:.1}us | period p50={:.1}us p90={:.1}us p99={:.1}us min={:.1}us max={:.1}us | wait p50={:.1}us p99={:.1}us min={:.1}us max={:.1}us | req={} | flat={} chunk={} sleep_fb={} early={} topup={} spin={} spin_avg={:.1}us | hist <600={} 600-900={} 900-1100={} 1100-1500={} 1500-2500={} >2500={}",
        t_ms / 1000,
        t_ms % 1000,
        FRAMES.load(Ordering::Relaxed),
        if mean > 0 { 10_000_000 / mean } else { 0 },
        hns_us(mean),
        hns_us(pct(&periods, 50)),
        hns_us(pct(&periods, 90)),
        hns_us(pct(&periods, 99)),
        hns_us(periods[0]),
        hns_us(periods[periods.len() - 1]),
        hns_us(pct(&waits, 50)),
        hns_us(pct(&waits, 99)),
        hns_us(waits.first().copied().unwrap_or(0)),
        hns_us(waits.last().copied().unwrap_or(0)),
        req_text,
        N_FLAT.load(Ordering::Relaxed),
        N_CHUNK.load(Ordering::Relaxed),
        N_SLEEP.load(Ordering::Relaxed),
        N_EARLY.load(Ordering::Relaxed),
        N_TOPOUT.load(Ordering::Relaxed),
        n_spin,
        spin_avg,
        hist[0],
        hist[1],
        hist[2],
        hist[3],
        hist[4],
        hist[5],
    );

    if let Some((hostf, simf, ft)) = engine_stats() {
        let prev_hostf = ENG_HOSTF.swap(hostf, Ordering::Relaxed);
        let prev_simf = ENG_SIMF.swap(simf, Ordering::Relaxed);
        let last = LAST_REPORT.load(Ordering::Relaxed);
        let _ = last;
        let df = hostf.wrapping_sub(prev_hostf) as f64;
        let ds = simf.wrapping_sub(prev_simf) as f64;
        let sim_factor = f64::from(ft) * df; // host_frametime * frames == simulated seconds per second
        line.push_str(&format!(
            " | engine hostFrames={} dFrames={} host_frametime={}us simFrames={} dSim={} simFactor={:.3}",
            hostf,
            df as u64,
            (f64::from(ft) * 1_000_000.0) as i64,
            simf,
            ds as u64,
            sim_factor,
        ));
    }

    line.push_str(&format!(
        " | timer_resolution={} throttled={}",
        match timer_resolution() {
            Some((now_us, _, _)) => format!("{}us", now_us),
            None => "n/a".to_string(),
        },
        match throttled() {
            Some(true) => "1",
            Some(false) => "0",
            None => "?",
        }
    ));

    let n_err = N_ERR.load(Ordering::Relaxed);
    if n_err > 0 {
        line.push_str(&format!(
            " | err={} last=0x{:08X}",
            n_err,
            LAST_ERR.load(Ordering::Relaxed)
        ));
    }

    log(&line);
}

/// Called from `fps1000_uninstall`.
pub fn on_unload() {
    let n_spin = N_SPIN.load(Ordering::Relaxed);
    let spin_sum = SPIN_SUM.load(Ordering::Relaxed);
    log(&format!(
        "diag: unloaded after {} frames, flat={} chunk={} sleep_fb={} early={} topup={} spin={} spin_avg={:.1}us",
        FRAMES.load(Ordering::Relaxed),
        N_FLAT.load(Ordering::Relaxed),
        N_CHUNK.load(Ordering::Relaxed),
        N_SLEEP.load(Ordering::Relaxed),
        N_EARLY.load(Ordering::Relaxed),
        N_TOPOUT.load(Ordering::Relaxed),
        n_spin,
        if n_spin > 0 {
            hns_us(spin_sum / n_spin as i64)
        } else {
            0.0
        },
    ));
}

fn elapsed_ms() -> i64 {
    match (unsafe { now_hns() }, START.load(Ordering::Relaxed)) {
        (Some(now), start) if start > 0 => (now - start) / 10_000,
        _ => 0,
    }
}

/// Convert 100 ns ticks (the unit the plugin measures in) to microseconds for the log.
fn hns_us(hns: i64) -> f64 {
    hns as f64 / 10.0
}

/// `engine.dll` counters, at fixed offsets of the retail build (image base 0x10000000):
/// `hostFrames` 0x10695D64, `simFrames` 0x10695DFC, `host_frametime` 0x10695E18.
fn engine_stats() -> Option<(u32, u32, f32)> {
    unsafe {
        let module = GetModuleHandleA(PCSTR(c"engine.dll".as_ptr().cast())).ok()?;
        if module.is_invalid() {
            return None;
        }
        let base = module.0 as *const u8;
        let hostf = core::ptr::read_unaligned(base.add(0x695D64).cast::<u32>());
        let simf = core::ptr::read_unaligned(base.add(0x695DFC).cast::<u32>());
        let ft = core::ptr::read_unaligned(base.add(0x695E18).cast::<f32>());
        if !ft.is_finite() || !(0.0..=1.0).contains(&ft) {
            return None;
        }
        Some((hostf, simf, ft))
    }
}

/// System timer resolution in 100 ns units: (current, minimum, maximum).
fn timer_resolution() -> Option<(i64, i64, i64)> {
    type Query = unsafe extern "system" fn(*mut u32, *mut u32, *mut u32) -> i32;
    unsafe {
        let module = GetModuleHandleA(PCSTR(c"ntdll.dll".as_ptr().cast())).ok()?;
        let proc = GetProcAddress(module, PCSTR(c"NtQueryTimerResolution".as_ptr().cast()))?;
        let query: Query = core::mem::transmute(proc);
        let (mut cur, mut min, mut max) = (0u32, 0u32, 0u32);
        if query(&raw mut cur, &raw mut min, &raw mut max) < 0 {
            return None;
        }
        Some((
            i64::from(cur) / 10,
            i64::from(min) / 10,
            i64::from(max) / 10,
        ))
    }
}

/// Whether `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION` is set for this process.
fn throttled() -> Option<bool> {
    #[repr(C)]
    struct State {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }
    let mut state = State {
        version: 1,
        control_mask: 0,
        state_mask: 0,
    };
    unsafe {
        GetProcessInformation(
            GetCurrentProcess(),
            windows::Win32::System::Threading::ProcessPowerThrottling,
            (&raw mut state).cast::<c_void>(),
            core::mem::size_of::<State>() as u32,
        )
        .ok()?;
    }
    Some(state.state_mask & 0x4 != 0)
}
