# fps1000 — ~1000 FPS for the Left 4 Dead 2 Windows dedicated server

A tiny [Metamod:Source](https://www.metamodsource.net/) plugin (pure Rust, one DLL) that lifts the
Left 4 Dead 2 dedicated server on Windows from **64 / 512 FPS** to **~1000 FPS**, without touching
system timer resolution and without busy-waiting.

- no `timeBeginPeriod`, no `NtSetTimerResolution`, no power-throttling tweaks;
- no spinning — CPU usage stays around 2–3 % on a modern desktop CPU;
- no external injector process; the patch is applied when Metamod loads the plugin and reverted
  when it unloads.

Measured on an i7-14700KF (Win11 26220, empty server, `c1m1_hotel`, tickrate 30): **983 FPS**,
`host_frametime` 1.00 ms, CPU 2.2 %. On a Xeon E5-2696 v4 (tickrate 100, empty server):
**927.0 FPS**, CPU 8.4 %.

`host_frametime` is deliberately kept at or above 1 ms: the engine floors it at 1 ms and advances
the simulation by that value, so a faster loop would make the game run faster than real time. See
[How the plugin works](#how-the-plugin-works).

## Why the cap exists

The dedicated server's launcher loop lives in `dedicated.dll` (`0x10003500` on the retail build)
and calls, once per frame:

```asm
MOV  ECX, [0x100de3e0]     ; g_pSys
MOV  EAX, [EDX+8]          ; Sys vtable slot 2 = Sleep
PUSH 1                     ; 1 ms
CALL EAX                   ; Sys->Sleep(1)
```

`Sys::Sleep` is a single jump — `55 8B EC 5D FF 25 <KERNEL32!Sleep>` — so every frame goes
straight into **`kernel32!Sleep`**, not into tier0's `ThreadSleep`. Windows quantizes `Sleep(1)`:

| Timer resolution in effect | Actual `Sleep(1)` | FPS ceiling |
|---|---|---|
| default (15.625 ms) | ~15.6 ms | **64** |
| 1 ms (`timeBeginPeriod(1)`) | ~1.95 ms | **512** |
| high-resolution waitable timer | ~1.0 ms | **~1000** |

Linux has no such step — `usleep(1000)` really sleeps about a millisecond — which is why the same
server reaches ~1000 FPS there. Engine timekeeping itself is *not* the problem:
`Sys_FloatTime` is `tier0!Plat_FloatTime` is `QueryPerformanceCounter`.

Windows 10 2004+ made timer resolution per-process, and recent Windows 11 builds go further:
about three seconds after startup the process gets `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`,
after which `timeBeginPeriod(1)` is silently revoked (it still returns success). So even the
"512 FPS" step is not reliable anymore.

## How the plugin works

`dedicated.dll` imports `Sleep` once, so all of its `Sleep` calls share a single IAT slot. On
`Load()` the plugin locates that slot by walking the PE import directory (the RVA is **not**
hardcoded — it is resolved by name, so engine updates do not break it), makes the slot writable
and points it at its own stub:

| requested `ms` | behaviour |
|---|---|
| `0` | return immediately (yield) |
| `>= 50` | call `kernel32!Sleep` as before (startup uses 40 ms, shutdown 200 ms) |
| anything else (the main loop's `1`) | wait on a `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer until a `QueryPerformanceCounter` deadline has really passed |

The wait is **deadline-verified**, and that matters more than it sounds: `engine.dll` floors
`host_frametime` at 1 ms (`host_frametime = max(dt, 0.001)` in `FUN_1018dbb0`) and advances the
simulation by `host_frametime` per host frame. A loop that iterates faster than once per
millisecond therefore makes game time run faster than wall-clock time — the server reports more
than 1000 FPS and the game runs at more than 1x speed.

A fixed request cannot prevent that, because the timer's rounding is machine-dependent: a 0.5 ms
request wakes after 1.0 ms on one host and after 0.67 ms on another. The stub therefore waits in
chunks of at most 0.5 ms and returns only once the deadline has elapsed, so the server settles at
roughly 950–1000 FPS and never runs faster than real time.

The timer is created lazily inside the stub, so a server that never sleeps never pays for it, and
the plugin never changes any process-wide or system-wide setting.

## Build

Requirements: a Rust toolchain with the `i686-pc-windows-msvc` target (MSVC is **not** needed for
the plugin itself — only for the linker that ships with the Rust toolchain).

```powershell
rustup target add i686-pc-windows-msvc
powershell -ExecutionPolicy Bypass -File build.ps1
```

`build.ps1` builds all three PLAPI variants and copies them to `dist\`:

| file | Metamod:Source |
|---|---|
| `dist\fps1000.dll` | 1.12.x (PLAPI 16) |
| `dist\fps1000_pl17.dll` | 2.0.0-dev (PLAPI 17, e.g. build 1410) |
| `dist\fps1000_pl18.dll` | 2.0.x master (PLAPI 18) |

To build a single variant by hand:

```powershell
cd core
cargo build --release --no-default-features --features plapi16   # or plapi17 / plapi18
# output: target\i686-pc-windows-msvc\release\fps1000.dll
```

The DLL is self-contained: static CRT, no VC++ redistributable, imports only `kernel32.dll`,
`ntdll.dll` and `api-ms-win-core-synch-l1-2-0.dll`.

## Install

### From a release package (recommended)

Download the zip that matches your Metamod build and extract it into the dedicated server
directory (the one containing `srcds.exe`):

| archive | Metamod:Source |
|---|---|
| `fps1000-0.1.0-plapi16.zip` | 1.12.x (PLAPI 16) |
| `fps1000-0.1.0-plapi17.zip` | 2.0.0-dev (PLAPI 17, e.g. build 1410) |
| `fps1000-0.1.0-plapi18.zip` | 2.0.x master (PLAPI 18) |

The archive already contains the `addons` tree, so extracting it at the server root gives you:

```
addons\fps1000\INSTALL.txt
addons\fps1000\bin\fps1000.dll
addons\fps1000\bin\fps1000.2.l4d2.dll
```

Then append one line to `addons\metamod\metaplugins.ini` and restart the server:

```
addons/fps1000/bin/fps1000
```

The archive intentionally does **not** ship `metaplugins.ini`, so an existing plugin list is
never overwritten.

### Manual layout

Pick the DLL that matches your Metamod build (see the table above) and copy it into the
dedicated server directory as `fps1000.dll`:

```
left4dead2\addons\metamod.vdf                        # official Metamod:Source
left4dead2\addons\metamod\...                        # official Metamod:Source files
left4dead2\addons\fps1000\bin\fps1000.dll            # this plugin
left4dead2\addons\fps1000\bin\fps1000.2.l4d2.dll     # same file, compatibility name
left4dead2\addons\metamod\metaplugins.ini            # add one line: addons/fps1000/bin/fps1000
```

## Verify

The plugin writes `fps1000.log` next to `srcds.exe`:

```
[fps1000] install: OK dedicated.dll=0x6BB20000 IAT[Sleep]=0x6BBD5164 saved=0x7581D770 stub=0x6A101080
```

Then read the engine's own frame counter (the server is a 32-bit process, so use 32-bit
PowerShell — `C:\Windows\SysWOW64\WindowsPowerShell\v1.0\powershell.exe`):

- `hostFrames` — `engine.dll + 0x695D64` (`0x10695D64` in image coordinates), frames per second;
- `host_frametime` — `engine.dll + 0x695E18`, seconds per frame (float).

`fps_max` must be **0**, not 1000: with `fps_max 1000`, `Host_FilterTime` skips frames shorter
than 1 ms, so the counter only counts executed frames and reads a misleading ~670/s.

## Notes and caveats

- An empty server hibernates by default (`sv_hibernate_when_empty 1`) and the loop then sleeps
  20 ms per frame, so FPS stays around 50 until a player joins. Set `sv_hibernate_when_empty 0`
  if you want full speed while empty (at the cost of idle CPU).
- `fps_max` is pinned to 30 at startup by the engine (it is forced back even with `+fps_max 0` on
  the command line); set it after boot with `sm_cvar fps_max 0` or `rcon fps_max 0`. This plugin
  does not touch that.
- Unload with `meta unload fps1000` or by removing the line from `metaplugins.ini`; `Unload()`
  restores the original IAT slot.
- Windows-only, dedicated-server-only. The client is a different binary and was not tested.
- Verified against Metamod:Source 1.12.0-git1228 (PLAPI 16) and 2.0.0-dev+1410 (PLAPI 17).

## License

MIT — see [LICENSE](LICENSE).
