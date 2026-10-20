//! Pure-Rust implementation of the Metamod:Source plugin ABI.
//!
//! There are only two contracts between MM:S and a plugin:
//!   1. export `CreateInterface(const char *name, int *code)` (cdecl), returning the plugin
//!      singleton when `name == "ISmmPlugin"`;
//!   2. the first four bytes of that singleton are a vtable pointer, and the vtable order must
//!      match the `ISmmPlugin` MM:S was compiled with.
//!
//! The layout below is transcribed slot by slot from `core/ISmmPlugin.h` of
//! alliedmodders/metamod-source, tag `1.12.0.1228` (16 slots):
//!
//! | slot | method | signature |
//! |------|--------|-----------|
//! | 0  | GetApiVersion | `int (this)` |
//! | 1  | ~ISmmPlugin (MSVC scalar-deleting destructor) | `void *(this, unsigned int)` |
//! | 2  | Load | `bool (this, PluginId, ISmmAPI *, char *, size_t, bool)` |
//! | 3  | AllPluginsLoaded | `void (this)` |
//! | 4  | QueryRunning | `bool (this, char *, size_t)` |
//! | 5  | Unload | `bool (this, char *, size_t)` |
//! | 6  | Pause | `bool (this, char *, size_t)` |
//! | 7  | Unpause | `bool (this, char *, size_t)` |
//! | 8  | GetAuthor | `const char *(this)` |
//! | 9  | GetName | `const char *(this)` |
//! | 10 | GetDescription | `const char *(this)` |
//! | 11 | GetURL | `const char *(this)` |
//! | 12 | GetLicense | `const char *(this)` |
//! | 13 | GetVersion | `const char *(this)` |
//! | 14 | GetDate | `const char *(this)` |
//! | 15 | GetLogTag | `const char *(this)` |
//!
//! MSVC member functions use `__thiscall` (`this` in ECX, the rest like stdcall with the callee
//! popping the stack), which maps exactly to Rust's `extern "thiscall"` (stable ABI on i686).

use core::ffi::{c_char, c_void};
use core::ptr;

/// Interface name MM:S uses to query the plugin.
const METAMOD_PLAPI_NAME: &[u8] = b"ISmmPlugin\0";

/// Return codes for `CreateInterface`.
const META_IFACE_OK: i32 = 0;
const META_IFACE_FAILED: i32 = 1;

/// PLAPI version reported by the plugin.
///
/// * `16` = Metamod:Source 1.12.x (`METAMOD_PLAPI_VERSION`)
/// * `17` = Metamod:Source 2.0.0-dev (the highest value build 1410 accepts)
/// * `18` = Metamod:Source 2.0.x master
///
/// Switch with `--no-default-features --features plapi16` etc.; the vtable layout is identical
/// for all three.
pub const PLAPI_VERSION: i32 = if cfg!(feature = "plapi18") {
    18
} else if cfg!(feature = "plapi16") {
    16
} else {
    17
};

// ---------------------------------------------------------------------------
// Vtable
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct PluginVTable {
    get_api_version: unsafe extern "thiscall" fn(*mut c_void) -> i32,
    destructor: unsafe extern "thiscall" fn(*mut c_void, u32) -> *mut c_void,
    load: unsafe extern "thiscall" fn(
        *mut c_void,
        i32,
        *mut c_void,
        *mut c_char,
        usize,
        bool,
    ) -> bool,
    all_plugins_loaded: unsafe extern "thiscall" fn(*mut c_void),
    query_running: unsafe extern "thiscall" fn(*mut c_void, *mut c_char, usize) -> bool,
    unload: unsafe extern "thiscall" fn(*mut c_void, *mut c_char, usize) -> bool,
    pause: unsafe extern "thiscall" fn(*mut c_void, *mut c_char, usize) -> bool,
    unpause: unsafe extern "thiscall" fn(*mut c_void, *mut c_char, usize) -> bool,
    get_author: unsafe extern "thiscall" fn(*mut c_void) -> *const c_char,
    get_name: unsafe extern "thiscall" fn(*mut c_void) -> *const c_char,
    get_description: unsafe extern "thiscall" fn(*mut c_void) -> *const c_char,
    get_url: unsafe extern "thiscall" fn(*mut c_void) -> *const c_char,
    get_license: unsafe extern "thiscall" fn(*mut c_void) -> *const c_char,
    get_version: unsafe extern "thiscall" fn(*mut c_void) -> *const c_char,
    get_date: unsafe extern "thiscall" fn(*mut c_void) -> *const c_char,
    get_log_tag: unsafe extern "thiscall" fn(*mut c_void) -> *const c_char,
}

/// The plugin singleton: just a vtable pointer; all state lives in crate statics.
#[repr(C)]
pub struct PluginObject {
    vtable: *const PluginVTable,
}

// The singleton itself is immutable and lives in read-only memory, so sharing it across threads
// is sound.
unsafe impl Sync for PluginObject {}

static VTABLE: PluginVTable = PluginVTable {
    get_api_version,
    destructor,
    load,
    all_plugins_loaded,
    query_running,
    unload,
    pause,
    unpause,
    get_author,
    get_name,
    get_description,
    get_url,
    get_license,
    get_version,
    get_date,
    get_log_tag,
};

static PLUGIN: PluginObject = PluginObject { vtable: &VTABLE };

// ---------------------------------------------------------------------------
// Method implementations
// ---------------------------------------------------------------------------

unsafe extern "thiscall" fn get_api_version(_this: *mut c_void) -> i32 {
    PLAPI_VERSION
}

/// MSVC scalar-deleting destructor: MM:S never deletes the plugin singleton, so just return
/// `this` unchanged.
unsafe extern "thiscall" fn destructor(this: *mut c_void, _flags: u32) -> *mut c_void {
    this
}

unsafe extern "thiscall" fn load(
    _this: *mut c_void,
    _id: i32,
    _ismm: *mut c_void,
    error: *mut c_char,
    maxlen: usize,
    _late: bool,
) -> bool {
    let rc = crate::fps1000_install();
    if rc == 0 {
        true
    } else {
        unsafe {
            set_error(
                error,
                maxlen,
                &format!("fps1000: install failed (rc={rc}), see fps1000.log next to srcds.exe"),
            );
        }
        false
    }
}

unsafe extern "thiscall" fn all_plugins_loaded(_this: *mut c_void) {}

unsafe extern "thiscall" fn query_running(
    _this: *mut c_void,
    _error: *mut c_char,
    _maxlen: usize,
) -> bool {
    true
}

unsafe extern "thiscall" fn unload(
    _this: *mut c_void,
    error: *mut c_char,
    maxlen: usize,
) -> bool {
    let rc = crate::fps1000_uninstall();
    if rc != 0 && rc != 1 {
        unsafe {
            set_error(error, maxlen, &format!("fps1000: uninstall failed (rc={rc})"));
        }
        return false;
    }
    true
}

unsafe extern "thiscall" fn pause(
    _this: *mut c_void,
    _error: *mut c_char,
    _maxlen: usize,
) -> bool {
    true
}

unsafe extern "thiscall" fn unpause(
    _this: *mut c_void,
    _error: *mut c_char,
    _maxlen: usize,
) -> bool {
    true
}

static AUTHOR: &[u8] = b"MoRanYue\0";
static NAME: &[u8] = b"fps1000\0";
static DESCRIPTION: &[u8] = b"Replace dedicated.dll's per-frame Sleep(1) with a high-resolution waitable timer so the Windows dedicated server runs at ~1000 FPS without changing system timer resolution and without busy-waiting.\0";
static URL: &[u8] = b"https://github.com/MoRanYue/l4d2-fps1000\0";
static LICENSE: &[u8] = b"MIT\0";
static VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
static DATE: &[u8] = b"2026-10-21\0";
static LOG_TAG: &[u8] = b"fps1000\0";

unsafe extern "thiscall" fn get_author(_this: *mut c_void) -> *const c_char {
    AUTHOR.as_ptr().cast()
}

unsafe extern "thiscall" fn get_name(_this: *mut c_void) -> *const c_char {
    NAME.as_ptr().cast()
}

unsafe extern "thiscall" fn get_description(_this: *mut c_void) -> *const c_char {
    DESCRIPTION.as_ptr().cast()
}

unsafe extern "thiscall" fn get_url(_this: *mut c_void) -> *const c_char {
    URL.as_ptr().cast()
}

unsafe extern "thiscall" fn get_license(_this: *mut c_void) -> *const c_char {
    LICENSE.as_ptr().cast()
}

unsafe extern "thiscall" fn get_version(_this: *mut c_void) -> *const c_char {
    VERSION.as_ptr().cast()
}

unsafe extern "thiscall" fn get_date(_this: *mut c_void) -> *const c_char {
    DATE.as_ptr().cast()
}

unsafe extern "thiscall" fn get_log_tag(_this: *mut c_void) -> *const c_char {
    LOG_TAG.as_ptr().cast()
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

/// MM:S finds this through `GetProcAddress(hModule, "CreateInterface")`.
///
/// # Safety
///
/// `name` must be a NUL-terminated C string or null; `code` must be four writable bytes or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn CreateInterface(name: *const c_char, code: *mut i32) -> *mut c_void {
    if !code.is_null() {
        unsafe { *code = META_IFACE_OK };
    }

    if !name.is_null() {
        let mut i = 0usize;
        loop {
            if i >= METAMOD_PLAPI_NAME.len() {
                break;
            }
            let got = unsafe { *name.add(i) } as u8;
            let want = METAMOD_PLAPI_NAME[i];
            if got != want {
                break;
            }
            if got == 0 {
                // exact name match
                return &PLUGIN as *const PluginObject as *mut c_void;
            }
            i += 1;
        }
    }

    if !code.is_null() {
        unsafe { *code = META_IFACE_FAILED };
    }
    ptr::null_mut()
}

/// Copy a message into the error buffer supplied by MM:S (UTF-8 + NUL, truncated if too long).
unsafe fn set_error(error: *mut c_char, maxlen: usize, msg: &str) {
    if error.is_null() || maxlen == 0 {
        return;
    }
    let bytes = msg.as_bytes();
    let n = bytes.len().min(maxlen - 1);
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), error.cast::<u8>(), n);
        *error.add(n) = 0;
    }
}
