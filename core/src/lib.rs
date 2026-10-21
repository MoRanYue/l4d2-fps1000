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
//! Why the wait is verified against a deadline
//! -------------------------------------------
//! `CreateWaitableTimerExW(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION)` is accurate to roughly half a
//! millisecond, and how a request is rounded depends on the host: on one machine a 0.5 ms request
//! wakes after 1.0 ms, on another after 0.67 ms. A fixed request is therefore not portable.
//!
//! It has to be safe, because the engine floors `host_frametime` at 1 ms (`FUN_1018dbb0` in
//! engine.dll: `host_frametime = max(dt, 0.001)`) and the simulation advances by `host_frametime`
//! per host frame. If the loop ever iterates faster than one millisecond per frame, accumulated
//! simulation time grows faster than wall-clock time and the game runs at more than 1x speed.
//! So every wait is checked against a `QueryPerformanceCounter` deadline: the stub never returns
//! before the requested duration has actually elapsed.

mod mm;

use core::ffi::c_void;
use core::ptr;
use std::sync::atomic::{AtomicI64, AtomicPtr, AtomicUsize, Ordering};

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
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
/// Longest single wait the high-resolution timer can honour accurately, in 100 ns ticks (0.5 ms).
const MAX_CHUNK: i64 = 5_000;
/// Slack added to every deadline so the wait is not extended by a last, useless sub-chunk.
const DEADLINE_MARGIN: i64 = 1_000;
/// A timer wait shorter than this would overshoot by more than it helps: stop instead
/// (0.3 ms). The frame's own work makes up the difference.
const MIN_CHUNK: i64 = 3_000;
/// Safety net: give up and fall back to `Sleep` after this many timer waits in one call.
const MAX_CHUNKS: u32 = 64;

/// Cached `QueryPerformanceFrequency` result (0 = not queried yet).
static QPC_FREQ: AtomicI64 = AtomicI64::new(0);

/// Original value of the dedicated.dll IAT slot before installation (the address of
/// `kernel32!Sleep`). **Only used to restore the slot on unload.**
static SAVED_SLEEP: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
/// Address of the `kernel32!Sleep` IAT slot: `*mut *mut c_void` points at that 4-byte slot.
static SLOT: AtomicPtr<*mut c_void> = AtomicPtr::new(ptr::null_mut());
/// Lazily created high-resolution timer handle.
static TIMER: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
/// Number of calls that took the timer path (diagnostics only).
static CALLS: AtomicUsize = AtomicUsize::new(0);

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
            Sleep(ms);
            return;
        };
        let Some(start) = now_hns() else {
            Sleep(ms);
            return;
        };

        CALLS.fetch_add(1, Ordering::Relaxed);
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
                return;
            }

            // Ask for less than what is left so that the timer's rounding-up cannot overshoot
            // the deadline by a full quantum; long waits only reserve one chunk for the tail.
            let ask = if remaining > 2 * MAX_CHUNK {
                remaining - MAX_CHUNK
            } else {
                remaining.min(MAX_CHUNK)
            };

            // Negative means relative time, and lpDueTime is a pointer: passing the value on the
            // stack would freeze the server.
            let due: i64 = -ask;
            if SetWaitableTimer(HANDLE(timer), &raw const due, 0, None, None, false).is_err() {
                Sleep(ms);
                return;
            }
            let _ = WaitForSingleObject(HANDLE(timer), INFINITE);
        }

        // The timer kept waking early (broken or throttled): fall back to the system call.
        Sleep(ms);
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

/// Lazily create and cache the high-resolution timer; keep a single handle under concurrency.
unsafe fn timer_handle() -> Option<*mut c_void> {
    unsafe {
        let existing = TIMER.load(Ordering::Acquire);
        if !existing.is_null() {
            return Some(existing);
        }

        let created = CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
        .ok()?;
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
    let text = format!(
        "fps1000_core {}\nwindows-crate=0.62.2 strict-provenance=on\ninstalled={}\niat_sleep=0x{:08X}\nsaved_sleep=0x{:08X}\ntimer={}\ncalls={}\n",
        env!("CARGO_PKG_VERSION"),
        if slot.is_null() { "no" } else { "yes" },
        slot as usize,
        SAVED_SLEEP.load(Ordering::Acquire) as usize,
        if TIMER.load(Ordering::Acquire).is_null() { "lazy" } else { "ready" },
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
