//! fps1000_core — replace srcds's per-frame `Sleep(1)` with a high-resolution waitable timer.
//!
//! Design notes
//! ------------
//! * **Only the official `windows` crate (0.62.2)** is used: there are no hand-written
//!   `extern "system"` declarations. `HANDLE`, `HMODULE` and `PAGE_PROTECTION_FLAGS` are the
//!   bindings' newtypes and windows-rs guarantees the signatures.
//! * **Strict provenance**: addresses are kept as real pointers (`*const u8`,
//!   `*mut *mut c_void`). `usize` is never used as a pointer and pointers are never rebuilt with
//!   `usize as *const T`; the address of a local is taken with `&raw const` / `&raw mut` instead
//!   of creating a `&T` temporary.
//! * The only `as usize` casts are in log formatting: pointer -> integer exposes the provenance
//!   without ever casting back.
//! * **The fallback path calls `windows::Win32::System::Threading::Sleep` directly** (through
//!   this module's own IAT slot), so the original function pointer is neither saved nor
//!   transmuted. The import table is parsed only to locate the dedicated.dll IAT slot that is
//!   about to be rewritten, not to obtain the address of `Sleep`.
//!
//! How the per-frame wait is calibrated
//! ------------------------------------
//! `CreateWaitableTimerExW(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION)` rounds a request up to the
//! host's timer quantum, and that quantum differs between machines: a 0.5 ms request wakes after
//! 1.0 ms on one host and after 0.67 ms on another. A fixed request is therefore not portable.
//!
//! It also has to be *stable*. The engine floors `host_frametime` at 1 ms (`FUN_1018dbb0` in
//! engine.dll: `host_frametime = max(dt, 0.001)`) and advances the simulation by it, so a period
//! below 1 ms makes the game run faster than real time, and a period that alternates between two
//! values makes the tick timeline uneven (snapshots and sound events arrive in bursts).
//!
//! The first timer call therefore calibrates: it probes a range of requests and keeps the one
//! whose three probes land closest to 1 ms without falling below it. Every later call performs a
//! single wait of exactly that length, so the frame period stays flat. A `QueryPerformanceCounter`
//! check after each wait catches a grossly early wake-up (below 0.9 ms) and tops it up; ordinary
//! jitter of a few tens of microseconds is left alone, because topping it up would add a whole
//! timer quantum and reintroduce the very jitter this is meant to avoid.

mod mm;
#[cfg(feature = "diag")]
mod diag;

use core::ffi::c_void;
use core::ptr;
use std::sync::atomic::{AtomicI64, AtomicPtr, AtomicUsize, Ordering};

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(feature = "diag")]
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, Sleep, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};

/// Requests at or above this many milliseconds are passed straight to the original `Sleep`
/// (startup uses 40 ms, shutdown uses 200 ms).
const FALLBACK_MS: u32 = 50;
/// 100 ns ticks per millisecond (the unit `SetWaitableTimer` uses).
const HNS_PER_MS: i64 = 10_000;
/// A calibrated wait must average at least this (0.98 ms; the frame's own work covers the last
/// few microseconds) ...
const ACCEPT_LOW: i64 = 9_800;
/// ... and at most this (1.12 ms), so the loop stays near 1000 FPS.
const ACCEPT_HIGH: i64 = 11_200;
/// Calibration prefers a candidate whose probes spread no more than this (150 us). A busier host
/// cannot do better; if nothing qualifies, the closest candidate above 0.98 ms is used anyway.
const STABLE_SPREAD: i64 = 1_500;
/// Every frame must really last at least this long (1 ms). The engine clamps `host_frametime`
/// up to 1 ms, so a frame that returns earlier makes the simulation clock run faster than wall
/// time; a frame that returns later only costs FPS and keeps the clock exact.
const TARGET_HNS: i64 = 10_000;
/// How much of the frame the engine spends working between two `Sleep` calls, tracked while the
/// server runs. The wait is shortened by that much so the *frame* lands on one millisecond: the
/// engine turns the frame period into simulation time, so a frame that is consistently 4% long
/// makes the simulation run 4% slow.
static WORK_EMA: AtomicI64 = AtomicI64::new(0);

/// Moving average of what is left of a frame after its wait, in QPC ticks (negative when late).
static REST_EMA: AtomicI64 = AtomicI64::new(i64::MIN);
/// Cap on the work compensation (0.3 ms): a wild average must not push the wait below 0.7 ms.
const WORK_CAP: i64 = 3_000;
/// A small safety on the deadline. The engine clamps `host_frametime` up to 1 ms, so a frame
/// that returns even 10 us early makes the simulation clock run fast; the work estimate is only
/// an average, so the deadline is pushed out by this much to keep the short frames above 1 ms.
/// A hard floor of 1 ms after the wait backs this up (see `sleep_stub`), so an estimate that
/// overshoots after a busy stretch cannot produce a short frame either.
const SAFETY: i64 = 150;
/// Longest tail worth spinning for (0.6 ms). A longer tail is handed back to the timer first,
/// because spinning that long would burn a whole core.

/// The controller aims for a wait that leaves at most this much to spin (0.25 ms); a longer spin
/// means the request was a quantum short.
const SPIN_TARGET: i64 = 2_500;
/// Hard cap on spin iterations, so a counter that never advances cannot wedge the server.
const SPIN_MAX_ITERS: u32 = 4_000_000;
/// How many frame periods to average before the settled re-calibration runs.
const PERIOD_WINDOW: usize = 512;
/// Fallback path: longest single wait the timer honours accurately (0.5 ms).
const MAX_CHUNK: i64 = 5_000;
/// Fallback path: slack added to the deadline so the wait is not extended by a useless sub-chunk.
const DEADLINE_MARGIN: i64 = 1_000;
/// Fallback path: a wait shorter than this is not worth another timer round trip (0.3 ms).
const MIN_CHUNK: i64 = 3_000;
/// Fallback path: give up and call `Sleep` after this many timer waits in one call.
const MAX_CHUNKS: u32 = 64;
/// `REQ_HNS` value meaning "not calibrated yet"; 0 means "calibration failed, use the chunks".
const REQ_UNSET: i64 = i64::MIN;

/// Cached `QueryPerformanceFrequency` result (0 = not queried yet).
static QPC_FREQ: AtomicI64 = AtomicI64::new(0);
/// Calibrated wait length in 100 ns ticks: `REQ_UNSET` before the first timer call, 0 when
/// calibration found nothing stable (the chunked fallback is used instead).
static REQ_HNS: AtomicI64 = AtomicI64::new(REQ_UNSET);
/// Number of calibrated waits that ended early (diagnostics; forces the fallback when large).
static EARLY: AtomicUsize = AtomicUsize::new(0);

/// Original value of the dedicated.dll IAT slot before installation (the address of
/// `kernel32!Sleep`). **Only used to restore the slot on unload.**
static SAVED_SLEEP: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
/// Address of the `kernel32!Sleep` IAT slot: `*mut *mut c_void` points at that 4-byte slot.
static SLOT: AtomicPtr<*mut c_void> = AtomicPtr::new(ptr::null_mut());
/// Lazily created high-resolution timer handle.
static TIMER: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
/// Number of calls that took the timer path (diagnostics only).
static CALLS: AtomicUsize = AtomicUsize::new(0);
/// Entry timestamp of the previous paced call, or 0 when the last call was not a frame sleep.
static LAST_ENTRY: AtomicI64 = AtomicI64::new(0);
/// When the previous paced call stopped waiting, or 0 when no frame was completed. The gap to the
/// next entry is the engine's own per-frame work, which the deadline has to leave room for.
static LAST_EXIT: AtomicI64 = AtomicI64::new(0);
/// Sum of the frame periods observed since the last adjustment, and how many were summed.
static PERIOD_SUM: AtomicI64 = AtomicI64::new(0);
static PERIOD_N: AtomicUsize = AtomicUsize::new(0);
/// Sum of the measured wait durations, used with `PERIOD_SUM` to derive the per-frame work.
static WAIT_SUM: AtomicI64 = AtomicI64::new(0);
/// Set once the one-shot correction has run, so it happens only once.
static SETTLED: AtomicUsize = AtomicUsize::new(0);
/// Moving average of the measured wait, used to trim the request at runtime.
static WAIT_EMA: AtomicI64 = AtomicI64::new(0);
/// Calibrated waits since the last request adjustment.
static ADJUST_N: AtomicUsize = AtomicUsize::new(0);
/// Set once a request step overshot the frame, which ends the upward search.
/// Frames since the last request overshoot: stepping up again is allowed only after a while,
/// so one bad step does not freeze the request at a needlessly low level.

// ---------------------------------------------------------------------------
// Replacement stub
// ---------------------------------------------------------------------------

/// Replacement for `Sleep` installed into the IAT. The signature must match `kernel32!Sleep`
/// (`extern "system"` == stdcall).
///
/// The fallback path calls `kernel32!Sleep` directly (through fps1000.dll's own IAT, which is
/// unrelated to the rewritten slot inside dedicated.dll, so this cannot recurse).
unsafe extern "system" fn sleep_stub(ms: u32) {
    unsafe {
        // 0 means "yield"; waits of 50 ms or more are left to the system so that startup and
        // shutdown keep their original behaviour.
        if ms == 0 || ms >= FALLBACK_MS {
            Sleep(ms);
            return;
        }

        let Some(timer) = timer_handle() else {
            #[cfg(feature = "diag")]
            diag::note_path("sleep");
            Sleep(ms);
            return;
        };
        if now_hns().is_none() {
            #[cfg(feature = "diag")]
            diag::note_path("sleep");
            Sleep(ms);
            return;
        }

        CALLS.fetch_add(1, Ordering::Relaxed);
        pace(ms);

        let mut req = REQ_HNS.load(Ordering::Relaxed);
        if req == REQ_UNSET {
            req = calibrate(timer);
            REQ_HNS.store(req, Ordering::Relaxed);
        }

        // Calibrated: one flat wait per frame, then a short spin out of the tail.
        if req > 0 {
            let Some(start) = now_hns() else {
                Sleep(ms);
                return;
            };
            // The frame has to end exactly one millisecond after it started, and the engine's own
            // work of the *previous* frame is already inside that millisecond, so it is taken off
            // the deadline here. The estimate is a moving average: a frame that comes back under
            // 1 ms would be clamped up by the engine (the simulation would run fast), so it is
            // better to be a few microseconds long than short.
            let deadline = start + TARGET_HNS + SAFETY - WORK_EMA.load(Ordering::Relaxed).clamp(0, WORK_CAP);
            if !wait_once(timer, req) {
                #[cfg(feature = "diag")]
                diag::note_path("sleep");
                Sleep(ms);
                return;
            }
            let Some(after) = now_hns() else {
                return;
            };
            let elapsed = after - start;
            WAIT_SUM.fetch_add(elapsed, Ordering::Relaxed);
            #[cfg(feature = "diag")]
            {
                diag::note_path("flat");
                diag::set_wait(elapsed);
            }
            let rest = deadline - after;
            if rest < 0 {
                EARLY.fetch_add(1, Ordering::Relaxed);
            }
            adapt_request(elapsed, rest);

            // Hard floor. The engine clamps `host_frametime` up to 1 ms, so a frame that ends
            // under 1 ms makes the simulation run faster than real time -- that is exactly what
            // shows up as "sv above 1000" and as events (kill notices, sounds) arriving before the
            // action they belong to. The work estimate above is a moving average and can overshoot
            // after a busy stretch, so the last few microseconds are spun out rather than trusted.
            let target = deadline.max(start + TARGET_HNS);
            if target > after {
                let _spun = spin_until(target);
                #[cfg(feature = "diag")]
                {
                    diag::note_path("spin");
                    diag::set_spin(_spun);
                }
            }
            LAST_EXIT.store(now_hns().unwrap_or(target), Ordering::Relaxed);
            return;
        }

        chunked_wait(timer, ms);
    }
}

/// Frame-period tracking and the one-shot settled re-calibration.
///
/// The first calibration runs on the first timer call, i.e. while the server is still loading the
/// map. The host's timer behaves differently then (measured on the development host: a 500 us
/// request woke after 990 us under load but after 520 us once the map was up), so the request that
/// was picked is often one quantum short of the frame loop's regime and every frame then has to be
/// spun out. After `PERIOD_WINDOW` steady frames the calibration is therefore run once more.
unsafe fn pace(ms: u32) {
    unsafe {
        let Some(entry) = now_hns() else {
            return;
        };
        let prev = LAST_ENTRY.swap(entry, Ordering::Relaxed);
        if ms > 1 {
            // Not the frame loop (hibernation and friends). The entry is still recorded so the
            // next call measures a period that the range check below rejects; clearing it instead
            // would let any other thread that sleeps with `Sleep(2)` restart the measurement on
            // every frame and keep the counter from ever filling.
            return;
        }
        if prev == 0 {
            return;
        }
        let period = entry - prev;
        #[cfg(feature = "diag")]
        diag::record(period);
        // How much of the frame is engine work rather than waiting. This is measured directly as
        // the gap between the end of the previous wait and this call. Deriving it from the period
        // instead is degenerate once the spin pins the period to the target: the estimate then has
        // no fixed point and sticks wherever it started, which showed up as a permanent 0.5 ms
        // spin on every frame.
        let exit = LAST_EXIT.swap(0, Ordering::Relaxed);
        if exit != 0 && entry > exit {
            let work = entry - exit;
            if (0..=2 * WORK_CAP).contains(&work) {
                let prev_work = WORK_EMA.load(Ordering::Relaxed);
                WORK_EMA.store(prev_work - prev_work / 16 + work / 16, Ordering::Relaxed);
            }
        }
        // `period` is in 100 ns ticks: accept 1 ms..5 ms and ignore hitches and loading frames.
        if !(HNS_PER_MS..=5 * HNS_PER_MS).contains(&period) {
            return;
        }
        PERIOD_SUM.fetch_add(period, Ordering::Relaxed);
        let n = PERIOD_N.fetch_add(1, Ordering::Relaxed) + 1;
        if n < PERIOD_WINDOW || SETTLED.load(Ordering::Relaxed) != 0 {
            return;
        }
        let _ = PERIOD_SUM.swap(0, Ordering::Relaxed);
        let _ = WAIT_SUM.swap(0, Ordering::Relaxed);
        PERIOD_N.store(0, Ordering::Relaxed);
        SETTLED.store(1, Ordering::Relaxed);
        let req = REQ_HNS.load(Ordering::Relaxed);
        if req <= 0 {
            return;
        }

        // The first calibration runs while the server is still loading the map, when the host's
        // timer behaves differently from the idle frame loop (a 500 us request measured 990 us
        // under load on the development host, but only 520 us once the map was up). Calibrate once
        // more after the loop has settled: a request that is a quantum short would otherwise be
        // spun out on every single frame, and one that is a quantum long would cost the frame that
        // time.
        if let Some(timer) = timer_handle() {
            let fresh = calibrate(timer);
            if fresh > 0 && fresh != req {
                REQ_HNS.store(fresh, Ordering::Relaxed);
                log(&format!(
                    "timer: re-calibrated {}us -> {}us once the frame loop settled",
                    req / 10,
                    fresh / 10,
                ));
            }
        }
    }
}

/// Trim [`REQ_HNS`] while the server runs.
///
/// Calibration measures the wait in one context and the frame loop runs it in another: on the
/// development host the same 500 us request measured 990 us during calibration but 517 us in the
/// frame loop, which left the spin to cover 400 us of every frame. So the measured wait is tracked
/// with a moving average and the request is stepped until the wait itself lands just under the
/// 1 ms target. The spin keeps the frame period correct at every step, so a bad guess costs CPU,
/// not pacing.
fn adapt_request(elapsed: i64, rest: i64) {
    /// Step per adjustment (0.05 ms), the share of the frame the spin should cover, and how far
    /// the measured wait may sit from that point.
    const STEP: i64 = 500;
    /// A wait that ends this far past the deadline (0.1 ms) is still cheaper than spinning the
    /// same time out; only beyond it is the level below worth trying.
    const LATE: i64 = 1_000;
    const WINDOW: usize = 128;

    let prev = WAIT_EMA.load(Ordering::Relaxed);
    let ema = if prev == 0 {
        elapsed
    } else {
        prev - prev / 8 + elapsed / 8
    };
    WAIT_EMA.store(ema, Ordering::Relaxed);

    if ADJUST_N.fetch_add(1, Ordering::Relaxed) + 1 < WINDOW {
        return;
    }
    ADJUST_N.store(0, Ordering::Relaxed);

    let prev_rest = REST_EMA.load(Ordering::Relaxed);
    let rest_ema = if prev_rest == i64::MIN {
        rest
    } else {
        prev_rest - prev_rest / 8 + rest / 8
    };
    REST_EMA.store(rest_ema, Ordering::Relaxed);

    let req = REQ_HNS.load(Ordering::Relaxed);
    if req <= 0 {
        return;
    }
    // `rest_ema` is what is left of the frame after the wait: below zero the timer overshot the
    // deadline (the frame is late and the simulation falls behind), above the spin budget it
    // stopped a quantum short (the spin pays for it, and the level below would be worse). Keep it
    // inside the band; a bad guess costs CPU, not pacing.
    // The spin fills the rest, so the period stays at the target at every level.
    let next = if rest_ema < -LATE {
        (req - STEP).max(3_000)
    } else if rest_ema > SPIN_TARGET {
        (req + STEP).min(HNS_PER_MS)
    } else {
        req
    };
    if next != req {
        REQ_HNS.store(next, Ordering::Relaxed);
        // Start the average over: carrying the old level's samples into the new one made the
        // controller take a second step down after a single overshoot.
        WAIT_EMA.store(0, Ordering::Relaxed);
        REST_EMA.store(i64::MIN, Ordering::Relaxed);
        log(&format!(
            "timer: wait mean {}us, request {}us -> {}us",
            ema / 10,
            req / 10,
            next / 10,
        ));
    }
}

/// Fallback: wait in chunks of at most 0.5 ms until a `QueryPerformanceCounter` deadline has
/// really passed. Settles slower than a calibrated wait, but is always safe.
unsafe fn chunked_wait(timer: *mut c_void, ms: u32) {
    unsafe {
        #[cfg(feature = "diag")]
        diag::note_path("chunk");
        let Some(start) = now_hns() else {
            Sleep(ms);
            return;
        };
        let deadline = start + i64::from(ms) * HNS_PER_MS + DEADLINE_MARGIN;

        for _ in 0..MAX_CHUNKS {
            let Some(now) = now_hns() else {
                Sleep(ms);
                return;
            };
            let remaining = deadline - now;
            if remaining <= MIN_CHUNK {
                // Close enough: another timer wait would overshoot by more than it adds, and the
                // frame's own work covers the last fraction of a millisecond.
                #[cfg(feature = "diag")]
                diag::set_wait(now - start);
                return;
            }

            // Ask for less than what is left so that the timer's rounding-up cannot overshoot
            // the deadline by a full quantum; long waits only reserve one chunk for the tail.
            let ask = if remaining > 2 * MAX_CHUNK {
                remaining - MAX_CHUNK
            } else {
                remaining.min(MAX_CHUNK)
            };

            if !wait_once(timer, ask) {
                #[cfg(feature = "diag")]
                diag::note_path("sleep");
                Sleep(ms);
                return;
            }
        }

        // The timer kept waking early (broken or throttled): fall back to the system call.
        #[cfg(feature = "diag")]
        diag::note_path("sleep");
        Sleep(ms);
    }
}

/// Busy-wait until `deadline` (100 ns ticks) and return how long that took.
///
/// Only ever called for a tail of at most [`SPIN_MAX`], so the CPU cost stays small; the loop is
/// also capped, so a `QueryPerformanceCounter` that never advances cannot wedge the server.
unsafe fn spin_until(deadline: i64) -> i64 {
    unsafe {
        let Some(start) = now_hns() else { return 0 };
        for _ in 0..SPIN_MAX_ITERS {
            match now_hns() {
                Some(now) if now >= deadline => break,
                Some(_) => core::hint::spin_loop(),
                None => break,
            }
        }
        now_hns().unwrap_or(deadline) - start
    }
}

/// Arm the timer for `hns` of 100 ns ticks and block until it fires.
unsafe fn wait_once(timer: *mut c_void, hns: i64) -> bool {
    unsafe {
        // Negative means relative time, and lpDueTime is a pointer: passing the value on the
        // stack would freeze the server.
        let due: i64 = -hns;
        if let Err(_err) = SetWaitableTimer(HANDLE(timer), &raw const due, 0, None, None, false) {
            #[cfg(feature = "diag")]
            diag::note_error("SetWaitableTimer", _err.code().0 as u32);
            return false;
        }
        let _rc = WaitForSingleObject(HANDLE(timer), INFINITE);
        #[cfg(feature = "diag")]
        if _rc != WAIT_OBJECT_0 {
            diag::note_error("WaitForSingleObject", _rc.0);
        }
        true
    }
}

/// Measure how long one wait of `hns` really takes, in 100 ns ticks.
unsafe fn probe(timer: *mut c_void, hns: i64) -> Option<i64> {
    unsafe {
        let start = now_hns()?;
        if !wait_once(timer, hns) {
            return None;
        }
        Some(now_hns()? - start)
    }
}

/// Find the request that actually waits about one millisecond on this host.
///
/// Returns the calibrated length in 100 ns ticks, or 0 when no candidate is stable enough (the
/// caller then uses [`chunked_wait`]).
unsafe fn calibrate(timer: *mut c_void) -> i64 {
    /// Every request worth probing: the natural 1 ms first, then shorter ones (which round up to
    /// the host's quantum) and a few longer ones for hosts with a coarse timer.
    const CANDIDATES: [i64; 11] = [
        10_000, 9_000, 8_000, 7_000, 6_000, 5_000, 11_000, 12_000, 13_000, 15_000, 20_000,
    ];
    /// Probes per candidate. Enough to see the spread, few enough to finish in ~50 ms.
    const PROBES: i64 = 4;

    unsafe {
        let mut best = 0i64;
        let mut best_err = i64::MAX;
        let mut best_mean = 0i64;
        let mut best_spread = i64::MAX;

        // Best effort for hosts whose timer is too jittery for the strict check below: take the
        // request that lands closest to 1 ms as long as it does not come in under it.
        let mut any = 0i64;
        let mut any_err = i64::MAX;
        let mut any_mean = 0i64;
        let mut any_spread = 0i64;
        let mut table = String::new();
        // (mean, candidate) for every probe that completed, used to derive the timer quantum.
        let mut samples: Vec<(i64, i64)> = Vec::new();

        for cand in CANDIDATES {
            let mut lo = i64::MAX;
            let mut hi = i64::MIN;
            let mut sum = 0i64;
            let mut ok = true;
            for _ in 0..PROBES {
                match probe(timer, cand) {
                    Some(t) => {
                        lo = lo.min(t);
                        hi = hi.max(t);
                        sum += t;
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            let mean = sum / PROBES;
            let spread = hi - lo;
            table.push_str(&format!(" {}:{}", cand / 10, mean / 10));
            samples.push((mean, cand));

            #[cfg(feature = "diag")]
            diag::on_probe(
                cand,
                mean,
                lo,
                hi,
                if mean < ACCEPT_LOW {
                    "early"
                } else if mean > ACCEPT_HIGH {
                    "late"
                } else if spread > STABLE_SPREAD {
                    "unstable"
                } else {
                    "stable"
                },
            );

            if mean < ACCEPT_LOW {
                continue;
            }
            // Prefer the candidate that lands closest to exactly one millisecond.
            let err = (mean - HNS_PER_MS).abs();
            if mean <= ACCEPT_HIGH && spread <= STABLE_SPREAD && err < best_err {
                best_err = err;
                best = cand;
                best_mean = mean;
                best_spread = spread;
            }
            if err < any_err {
                any_err = err;
                any = cand;
                any_mean = mean;
                any_spread = spread;
            }
        }

        // The probe table also exposes the host's timer quantum: the step between the distinct
        // wake times (0.5 ms on most machines, 1 ms on a few, finer when something has raised
        // the resolution). Rounding first keeps measurement noise from looking like a step.
        let mut means: Vec<i64> = samples.iter().map(|(m, _)| (m / 100) * 100).collect();
        means.sort_unstable();
        means.dedup();
        let quantum = means
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|d| *d >= 100)
            .min()
            .unwrap_or(0);
        let quantum_text = if quantum > 0 {
            format!(", quantum ~{}us", quantum / 10)
        } else {
            String::new()
        };

        if best != 0 {
            log(&format!(
                "timer: request={}us wakes after {}.{:03}ms (spread {}us{})",
                best / 10,
                best_mean / HNS_PER_MS,
                (best_mean % HNS_PER_MS) / 10,
                best_spread / 10,
                quantum_text,
            ));
            #[cfg(feature = "diag")]
            diag::on_calibrated(best, "stable");
            best
        } else if any != 0 {
            log(&format!(
                "timer: request={}us wakes after {}.{:03}ms (spread {}us, jittery{})",
                any / 10,
                any_mean / HNS_PER_MS,
                (any_mean % HNS_PER_MS) / 10,
                any_spread / 10,
                quantum_text,
            ));
            #[cfg(feature = "diag")]
            diag::on_calibrated(any, "jittery");
            any
        } else {
            log(&format!(
                "timer: no request reached 0.98ms (request:mean us ={}), using the chunked wait",
                table
            ));
            #[cfg(feature = "diag")]
            diag::on_calibrated(0, "chunked");
            0
        }
    }
}

/// Current time in 100 ns ticks, from `QueryPerformanceCounter` (the same clock the engine uses).
unsafe fn now_hns() -> Option<i64> {
    unsafe {
        let mut freq = QPC_FREQ.load(Ordering::Relaxed);
        if freq <= 0 {
            let mut queried = 0i64;
            if QueryPerformanceFrequency(&raw mut queried).is_err() || queried <= 0 {
                return None;
            }
            QPC_FREQ.store(queried, Ordering::Relaxed);
            freq = queried;
        }

        let mut counter = 0i64;
        if QueryPerformanceCounter(&raw mut counter).is_err() {
            return None;
        }
        // 1 s == 10_000_000 ticks of 100 ns; i128 keeps the multiply from overflowing.
        Some(((i128::from(counter) * 10_000_000) / i128::from(freq)) as i64)
    }
}

/// Cached `QueryPerformanceFrequency` (diagnostics only).
#[cfg(feature = "diag")]
fn qpc_freq() -> Option<i64> {
    let cached = QPC_FREQ.load(Ordering::Relaxed);
    if cached > 0 {
        return Some(cached);
    }
    let _ = unsafe { now_hns() };
    let cached = QPC_FREQ.load(Ordering::Relaxed);
    if cached > 0 { Some(cached) } else { None }
}

/// Lazily create and cache the high-resolution timer; keep a single handle under concurrency.
unsafe fn timer_handle() -> Option<*mut c_void> {
    unsafe {
        let existing = TIMER.load(Ordering::Acquire);
        if !existing.is_null() {
            return Some(existing);
        }

        let created = match CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        ) {
            Ok(handle) => handle,
            Err(_err) => {
                #[cfg(feature = "diag")]
                diag::note_error("CreateWaitableTimerExW", _err.code().0 as u32);
                return None;
            }
        };
        if created.is_invalid() {
            return None;
        }

        match TIMER.compare_exchange(
            ptr::null_mut(),
            created.0,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Some(created.0),
            Err(prev) => {
                // Another thread installed its handle first; close ours.
                let _ = CloseHandle(created);
                if prev.is_null() { None } else { Some(prev) }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PE import table walk (locate the IAT slot for kernel32!Sleep)
// ---------------------------------------------------------------------------

#[inline]
unsafe fn rd_u16(p: *const u8) -> u16 {
    unsafe { ptr::read_unaligned(p.cast::<u16>()) }
}

#[inline]
unsafe fn rd_u32(p: *const u8) -> u32 {
    unsafe { ptr::read_unaligned(p.cast::<u32>()) }
}

/// Read a NUL-terminated ASCII string (at most 256 bytes); the slice borrows from the module
/// image.
unsafe fn read_cstr(p: *const u8) -> &'static [u8] {
    unsafe {
        let mut n = 0usize;
        while n < 256 && *p.add(n) != 0 {
            n += 1;
        }
        core::slice::from_raw_parts(p, n)
    }
}

fn ascii_eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Find the IAT slot of `dll!func` inside a module image and return the slot address
/// (`*mut *mut c_void`).
///
/// Only PE32 (x86 dedicated server) is supported; everything is pointer arithmetic, with no
/// `usize -> *const T` reconstruction.
unsafe fn find_iat_slot(
    base: *const u8,
    dll: &[u8],
    func: &[u8],
) -> Option<*mut *mut c_void> {
    unsafe {
        if base.is_null() || rd_u16(base) != 0x5A4D {
            return None; // "MZ"
        }

        let nt = base.add(rd_u32(base.add(0x3C)) as usize);
        if rd_u32(nt) != 0x0000_4550 {
            return None; // "PE\0\0"
        }

        let opt = nt.add(0x18);
        if rd_u16(opt) != 0x010B {
            return None; // PE32
        }

        let dir = opt.add(0x60);
        let imp_rva = rd_u32(dir.add(8)) as usize;
        let imp_size = rd_u32(dir.add(12)) as usize;
        if imp_rva == 0 || imp_size < 20 {
            return None;
        }

        let mut desc = base.add(imp_rva);
        let desc_end = desc.add(imp_size);
        while desc.add(20) <= desc_end {
            let oft = rd_u32(desc) as usize; // OriginalFirstThunk
            let name_rva = rd_u32(desc.add(12)) as usize;
            let ft = rd_u32(desc.add(16)) as usize; // FirstThunk (the IAT)
            if name_rva == 0 || ft == 0 {
                break;
            }

            if ascii_eq_ci(read_cstr(base.add(name_rva)), dll) {
                let thunks = base.add(if oft != 0 { oft } else { ft });
                let mut i = 0usize;
                while i < 4096 {
                    let thunk = rd_u32(thunks.add(i * 4)) as usize;
                    if thunk == 0 {
                        break;
                    }
                    // The high bit marks an import by ordinal, which has no name.
                    if thunk & 0x8000_0000 == 0 && ascii_eq_ci(read_cstr(base.add(thunk + 2)), func)
                    {
                        return Some(base.add(ft + i * 4).cast_mut().cast::<*mut c_void>());
                    }
                    i += 1;
                }
            }

            desc = desc.add(20);
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Exports called from the Metamod:Source layer (mm.rs)
// ---------------------------------------------------------------------------

/// Install the replacement. Returns 0 on success; 1 = dedicated.dll not found;
/// 2 = IAT slot not found; 3 = slot is null; 4 = VirtualProtect failed.
#[unsafe(no_mangle)]
pub extern "C" fn fps1000_install() -> i32 {
    unsafe {
        if !SLOT.load(Ordering::Acquire).is_null() {
            return 0; // already installed, idempotent
        }

        let module = match GetModuleHandleA(PCSTR(c"dedicated.dll".as_ptr().cast())) {
            Ok(m) if !m.is_invalid() => m.0,
            _ => {
                log("install: ERR dedicated.dll is not loaded (not a dedicated server?)");
                return 1;
            }
        };
        let base = module as *const u8;

        let Some(slot) = find_iat_slot(base, b"kernel32.dll", b"Sleep") else {
            log("install: ERR KERNEL32!Sleep not found in the import table");
            return 2;
        };

        let saved = ptr::read_unaligned(slot);
        if saved.is_null() {
            log("install: ERR the IAT slot is null");
            return 3;
        }

        let stub = sleep_stub as *const () as *mut c_void;

        let mut old = PAGE_PROTECTION_FLAGS(0);
        if VirtualProtect(slot.cast(), 4, PAGE_READWRITE, &raw mut old).is_err() {
            log("install: ERR VirtualProtect (make writable) failed");
            return 4;
        }
        ptr::write_unaligned(slot, stub);

        let mut restored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(slot.cast(), 4, old, &raw mut restored);

        SAVED_SLEEP.store(saved, Ordering::SeqCst);
        SLOT.store(slot, Ordering::SeqCst);

        log(&format!(
            "install: OK dedicated.dll=0x{:08X} IAT[Sleep]=0x{:08X} saved=0x{:08X} stub=0x{:08X}",
            base as usize, slot as usize, saved as usize, stub as usize
        ));
        #[cfg(feature = "diag")]
        diag::on_install();
        0
    }
}

/// Restore the IAT slot and close the timer. Returns 0 on success.
#[unsafe(no_mangle)]
pub extern "C" fn fps1000_uninstall() -> i32 {
    unsafe {
        let slot = SLOT.swap(ptr::null_mut(), Ordering::SeqCst);
        let saved = SAVED_SLEEP.swap(ptr::null_mut(), Ordering::SeqCst);
        if slot.is_null() || saved.is_null() {
            return 0;
        }

        let mut old = PAGE_PROTECTION_FLAGS(0);
        if VirtualProtect(slot.cast(), 4, PAGE_READWRITE, &raw mut old).is_err() {
            return 1;
        }
        ptr::write_unaligned(slot, saved);
        let mut restored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(slot.cast(), 4, old, &raw mut restored);

        let timer = TIMER.swap(ptr::null_mut(), Ordering::SeqCst);
        if !timer.is_null() {
            let _ = CloseHandle(HANDLE(timer));
        }

        log(&format!(
            "uninstall: OK IAT[Sleep] restored to 0x{:08X}",
            saved as usize
        ));
        #[cfg(feature = "diag")]
        diag::on_unload();
        0
    }
}

/// Write the current state into a caller-provided buffer (UTF-8 + NUL) and return the number of
/// bytes written, excluding the NUL.
///
/// # Safety
///
/// `buf` must point to at least `cap` writable bytes; when `cap == 0` or `buf` is null the
/// pointer is never dereferenced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fps1000_info(buf: *mut u8, cap: usize) -> usize {
    if buf.is_null() || cap == 0 {
        return 0;
    }

    let slot = SLOT.load(Ordering::Acquire);
    let req = REQ_HNS.load(Ordering::Relaxed);
    let req_text = if req == REQ_UNSET {
        "uncalibrated".to_string()
    } else {
        format!("{}.{:03}ms", req / HNS_PER_MS, (req % HNS_PER_MS) / 10)
    };
    let text = format!(
        "fps1000_core {}\nwindows-crate=0.62.2 strict-provenance=on\ninstalled={}\niat_sleep=0x{:08X}\nsaved_sleep=0x{:08X}\ntimer={}\nwait={}\nearly={}\ncalls={}\n",
        env!("CARGO_PKG_VERSION"),
        if slot.is_null() { "no" } else { "yes" },
        slot as usize,
        SAVED_SLEEP.load(Ordering::Acquire) as usize,
        if TIMER.load(Ordering::Acquire).is_null() { "lazy" } else { "ready" },
        req_text,
        EARLY.load(Ordering::Relaxed),
        CALLS.load(Ordering::Relaxed),
    );

    let bytes = text.as_bytes();
    let n = bytes.len().min(cap - 1);
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        *buf.add(n) = 0;
    }
    n
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn log(msg: &str) {
    use std::io::Write;

    let mut line = String::with_capacity(msg.len() + 32);
    line.push_str("[fps1000] ");
    line.push_str(msg);
    line.push('\n');

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `fps1000.log` next to `srcds.exe`.
fn log_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fps1000.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("fps1000.log"))
}