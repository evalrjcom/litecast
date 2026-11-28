# LiteCast Studio v1.3.3 — User & Developer Guide

[![CI](https://github.com/evalrjcom/litecast/actions/workflows/ci.yml/badge.svg)](https://github.com/evalrjcom/litecast/actions/workflows/ci.yml)

`litecast_v1.3.3` is a lightweight, OBS-style screen recorder and live streamer written in Rust with an FLTK interface. It ships as one source file (`src/main.rs`) with three crate dependencies (`fltk`, `serde`, `serde_json`), plus `wasapi` and `windows-sys` on Windows only and `libc` on Linux/macOS (local-time log stamps). It drives **ffmpeg** as an external worker for capture and encoding.

---

## 1. Features

| Area | What you get |
|---|---|
| Scenes | Unlimited* scenes; add / remove / rename / reorder; instant cut switching |
| Sources | Display capture (region or full screen, multi-monitor), webcam / video device, media file (loop), image (alpha preserved), solid colour |
| Layout | Z-order up/down, show/hide, X/Y/W/H, opacity 0–100 %, Fill / Center / 1:1 buttons |
| Preview | Live preview. Click to select, drag to move, Shift-drag (or grab the red corner handle) to resize |
| Recording | mkv (default, crash-safe), mp4 (faststart), mov, flv, ts. Never overwrites existing files |
| Streaming | **Kick, Rumble**, Twitch, YouTube and custom RTMP/RTMPS/SRT presets. **Multistream** to up to 8 destinations from one encode, with per-destination auto-reconnect so one failing platform never drops the others |
| Audio | Mic + desktop (system) audio, mixed. **Windows: native WASAPI capture, on by default, with no Stereo Mix needed.** Other backends: PulseAudio/PipeWire, ALSA, DirectShow, AVFoundation |
| Audio processing | **Mic compressor + noise gate + rumble filter**, **auto-levelling** of mic and desktop to one loudness target so neither is louder than the other, optional ducking of desktop audio while you talk, master limiter. **Live VU meters** for Mic, Desktop and Master. Audio Mixer dialog (Ctrl+M) |
| Encoders | libx264, NVENC, QuickSync, AMF, VAAPI, VideoToolbox (whichever your ffmpeg provides) |
| Look | **Dark theme** by default; Settings → Theme switches to Light (the classic look) or System (follows your Windows, macOS or GNOME setting). Applies instantly, no restart |
| Extras | PNG screenshot (F12), JSON profiles, headless `--record N` / `--stream N` modes for scheduled or cron use |

\*Hard caps of 64 scenes and 64 sources per scene protect against corrupt profiles.

---

## 2. Requirements

**Runtime**
- ffmpeg ≥ 4.4 on `PATH`, or set its path in Settings. Version 5.1 or later is recommended (uses `-fps_mode`; older builds fall back to `-vsync`).
- Linux: an X11 session. Wayland sessions work only through XWayland, because ffmpeg's `x11grab` cannot see native Wayland windows.
- Windows 10/11: ffmpeg built with gdigrab (gyan.dev or BtbN builds work). Audio is captured by LiteCast itself, so ffmpeg needs no audio device support.
- macOS: ffmpeg with AVFoundation, plus Screen Recording permission granted to the terminal or app.

**Build**
- Rust 1.88+ (stable, edition 2024), CMake ≥ 3.15, a C++17 compiler.
- Debian/Ubuntu packages:
  ```bash
  sudo apt install build-essential cmake ninja-build pkg-config \
    libx11-dev libxext-dev libxft-dev libxinerama-dev libxcursor-dev \
    libxrender-dev libxfixes-dev libpango1.0-dev libcairo2-dev \
    libgl1-mesa-dev libglu1-mesa-dev ffmpeg
  ```

---

## 3. Build & run

```bash
git clone https://github.com/evalrjcom/litecast && cd litecast
cargo build --release
./target/release/litecast
```

**Windows (native, MSVC)**: install Rust, CMake and VS Build Tools, then run `cargo build --release`. The release binary uses the GUI subsystem, so no console window appears.
**Windows (cross from Linux)**:
```bash
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

### Command line
```
litecast [--profile FILE] [--record SECONDS] [--stream SECONDS] [--version] [--help]
```
- `--profile FILE`: use a specific profile. Default: `~/.config/litecast/profile.json` on Linux and macOS, `%APPDATA%\litecast\profile.json` on Windows.
- `--record N`: start recording about 1.5 s after launch, stop after N seconds, finalise the file and exit. Useful with cron or Task Scheduler, e.g. `DISPLAY=:0 litecast --record 3600`.
- `--stream N`: same, but goes live to every enabled destination for N seconds. Both flags can be combined; the app exits when both finish.

---

## 4. Using it

1. **First launch** creates *Scene 1* with a full-screen Display Capture of the primary monitor, fitted to a 1920×1080 canvas.
2. **Add sources** with the **+** under *Sources*. Choose a type and fill in:
   - *Display*: the target is the X display (`:0.0`) on Linux, `desktop` on Windows (or `title=Window Name` to capture one window), or the screen index on macOS. Pick a monitor to auto-fill the region.
   - *Webcam*: press **Detect** to list devices. Size (e.g. `1280x720`) and FPS are optional.
   - *Media / Image*: use **Browse…**.
   - *Color*: pick a colour; use opacity for tints.
3. **Arrange** layers in the preview (drag to move, Shift-drag to resize) or type X/Y/W/H and press **Apply**. **Raise/Lower** change z-order. The list shows the top layer first.
4. **Settings** (Ctrl+,): canvas size, FPS, encoder, bitrate, recording folder and format, stream destinations, audio devices, ffmpeg path, and **Theme** (Dark / Light / System).
5. **Record** (Ctrl+R) and **Stream** (Ctrl+L) run independently and can run at the same time.
6. **Screenshot** (F12) saves a PNG of the composed canvas to the recording folder.
7. Profiles auto-save every 15 s when changed, and on exit. Ctrl+S saves now; File → Save As / Load handle other files.

### Keyboard shortcuts
| Key | Action |
|---|---|
| Ctrl+R | Toggle recording |
| Ctrl+L | Toggle streaming |
| F12 | Screenshot |
| Ctrl+, | Settings |
| Ctrl+D | Stream destinations |
| Ctrl+M | Audio Mixer (compressor, gate, levels, ducking) |
| Ctrl+S / Ctrl+Shift+S / Ctrl+O | Save / Save As / Load profile |
| Ctrl+Q | Quit (asks first while live; always finalises recordings) |

### Streaming to Kick, Rumble & others (multistream)
Open **Controls → Stream Destinations…** (Ctrl+D), or **Edit…** in Settings. Press **Add**, pick a service, and the server URL fills in automatically. Paste your stream key and press **OK**. Add as many destinations as you like (up to 8) and enable or disable each one. **Ctrl+L** then goes live to every enabled destination at once.

| Service | Default server URL | Where to get the key | Limits checked by LiteCast |
|---|---|---|---|
| **Kick** | `rtmps://fa723fc1b171.global-contribute.live-video.net/app` | Kick Creator Dashboard → stream settings (copy the URL too if it differs from the default) | RTMPS only, ≤ 8000 kbps, CBR, 2 s keyframes |
| **Rumble** | `rtmp://rtmp.rumble.com/live` | rumble.com → Live Stream / *Get Streamer Configuration*. Keys are per-event unless you enable a static key | keyframes ≤ 2 s recommended |
| Twitch | `rtmp://live.twitch.tv/app` | Creator Dashboard → Settings → Stream | 2 s keyframes |
| YouTube | `rtmp://a.rtmp.youtube.com/live2` | YouTube Studio → Go Live → Stream | 2 s keyframes |
| Custom | any `rtmp://`, `rtmps://` or `srt://host:port?streamid=…` | — | URL syntax only |

How it works and what to expect:
- **One encode, many outputs.** The canvas is encoded once and fanned out with ffmpeg's `tee` muxer. Each destination gets its own `fifo` queue, so CPU cost does not grow with the number of platforms.
- **Independent reconnects.** If one platform drops, only that output retries (every 2 s, up to 25 times, restarting on a keyframe). The others carry on. The status bar shows each one: `● LIVE 00:12:31 → Kick ✓, Rumble ⟳`, where ⟳ means it reported an error in the last 10 s.
- **Pre-flight checks** run when you save destinations or settings, and again on Go Live. Hard errors block the start: no enabled destination, an invalid URL, or an `rtmps://` URL when your ffmpeg lacks TLS support. Warnings are only logged: bitrate above a service limit, keyframe interval mismatch.
- **Live encoding** uses CBR (`nal-hrd=cbr`) with libx264, as Kick and Rumble require. Recommended for 1080p30: *veryfast*, 6000 kbps (stay ≤ 8000 for Kick), keyframe interval 2 s.
- **No audio configured?** A silent stereo track is added automatically while streaming, because several platforms, Kick included, reject video-only streams.
- Changes to destinations apply on the next Go Live.

### Audio notes
- **Linux (PulseAudio/PipeWire)**: Mic `default`, Desktop `@DEFAULT_MONITOR@`. **Detect devices** lists everything `pactl` knows about.
- **Windows (backend `wasapi`, the default)**: LiteCast captures the default microphone and the default speakers/headphones (*loopback*, i.e. everything you hear) itself. Both fields default to `default`. **Detect devices** lists every input (`[in]`) and output (`[out]`) by name: pick one, or clear a field to turn that source off. Unplugging a device or changing the Windows default is handled automatically: the source reopens within a few seconds. A source that plays nothing records as silence, not as a gap.
- **Upgrading from ≤ 1.1.1 on Windows**: profiles that used `dshow` with no devices (the old default, which recorded no audio) switch to `wasapi` automatically. A dshow device you had configured yourself is left unchanged.
- **Windows, dshow backend**: still available for capture cards. Desktop audio there needs a loopback device such as *Stereo Mix* or a virtual cable.
- Since 1.3.0 every backend goes through LiteCast's own mixer (ffmpeg-side backends are captured by a small ffmpeg helper per source), so processing and meters work the same everywhere. `amix` is no longer used.
### Audio mixer & VU meters (1.3.0)
The **Audio** panel (right of Controls) shows a meter per channel, updated 20× a second:

| Part of the meter | Meaning |
|---|---|
| Bar (green → yellow → red) | Loudness (RMS) on a −60…0 dBFS scale. Yellow from −20 dBFS, red from −9 dBFS |
| Thin white line | Instantaneous peak |
| Thicker tick that lingers | Peak hold (1.5 s); turns red above −1 dBFS (near clipping) |
| Blue marker | The auto-level **target** |
| Green dot left of the bar | Signal present (mic: gate open). No dot: silent or gated |
| Readout | Level in dB, `GR` = compressor/limiter gain reduction, `±dB` = what auto-level is currently applying; `muted` when muted |

**Audio Mixer… (Ctrl+M)**: changes apply live while the dialog is open, so you can watch the meters as you adjust. *OK* saves them to the profile, *Cancel* reverts and *Defaults* restores the factory values.

| Setting | Default | What it does |
|---|---|---|
| Target loudness | −20 dBFS | Level both mic and desktop are steered to. This is what keeps one from being louder than the other |
| Mic noise gate | on, −50 dB | Turns fan/keyboard hiss down 40 dB when you are not talking (200 ms hold, so word endings are kept) |
| Mic compressor | on, −26 dB, 3:1 | Evens out loud and soft speech (soft knee, 5 ms attack, 150 ms release) |
| Auto-level mic / desktop | on / on | Slow automatic gain, from −20 to +24 dB on the mic and −20 to +15 dB on desktop. It only adapts while there is signal, so silence and pauses are never boosted into hiss. It turns down faster (10 dB/s) than up (3 dB/s), and a sudden blast is clamped at 6 dB above the target |
| Duck desktop under mic | 0 dB (off) | Lowers desktop audio by this much while you speak (e.g. 10–12 dB for commentary over gameplay) |
| Trim mic / desktop | 0 dB | Manual offset after auto-level, if you want one consistently a bit louder |
| Mute mic / desktop | off | Silence a channel without changing devices |
| Master limiter | on | Brick-wall at −1 dBFS, so the mix never clips |

The mic always passes through an 80 Hz high-pass (desk thumps, rumble). Turn auto-level off on a channel if you want to set its level purely by hand with Trim.

- If a recording stops within seconds of starting, the cause is almost always a wrong audio device. Check the log, or set the backend to `none`.

---

## 5. Troubleshooting

| Symptom | Fix |
|---|---|
| Black display source on Linux | You are on pure Wayland. Log in to an X11 session, or capture an XWayland app |
| "ffmpeg NOT FOUND" in the status bar | Install ffmpeg or set the full path in Settings |
| Source shows `error:` under Transform | Read the Log panel. The source retries automatically (1 s → 10 s backoff) |
| `lagged` counter rising | Lower canvas/FPS, use a hardware encoder, or use a faster x264 preset |
| mp4 won't play after a crash | Use mkv (the default). It stays playable; remux later with `ffmpeg -i in.mkv -c copy out.mp4` |
| Mic and desktop at very different levels | Ctrl+M: make sure *Auto-level* is on for both. Lower *Target* if the Master meter shows constant limiter GR |
| Background hiss pumps between words | Lower the gate threshold (e.g. −55 dB) or raise it above the hiss level shown on the Mic meter while you are silent |
| No audio in recordings (Windows) | Settings → Audio backend `wasapi`, with Mic and Desktop set to `default` (or a device from **Detect devices**). The log shows `[audio] mic: capturing "…"` when it works |
| Status bar shows `⚠ audio −N ms` / log says *encoder is not keeping up* | The encoder fell more than 1 s behind, so audio was dropped and may drift. Use a faster x264 preset or a hardware encoder, or lower the canvas size/FPS |
| Log panel too narrow / window too dark or light | Update to 1.3.3 (the Log panel was squeezed since 1.3.0). Settings → Theme picks Dark, Light or System |
| Recording stops instantly with *Error parsing options for input file pipe:0* | Your ffmpeg is newer than LiteCast ≤ 1.1.0 expected. Update to 1.1.1 or later |
| Stream keeps reconnecting / ⟳ next to a destination | Check that destination's URL and key; the log shows the redacted ffmpeg error. Rumble keys are per-event, so generate a new one or enable a static key |
| "ffmpeg lacks RTMPS" on Go Live | Your ffmpeg was built without TLS. Install a full build (distro ffmpeg, gyan.dev or BtbN). Kick needs RTMPS |

---

## 6. Developer notes

### Architecture
```
capture ffmpeg ─BGRA─▶ reader thread ─▶ FrameSlot ┐
color / image ───────────────────────▶ FrameSlot ┤
                                                  ▼
                compositor thread (fixed FPS, z-order, alpha, opacity)
                  │ canvas Arc (pooled)        │ RGB preview (downscaled)
          writer thread  writer thread          FLTK preview widget
               │              │
        ffmpeg encoder  ffmpeg encoder  (+ ffmpeg-native audio inputs, or ↓)

Audio:  mic (WASAPI / ffmpeg helper) ─▶ ring ─▶ HPF ─▶ gate ─▶ compressor ─▶ auto-level ─▶ trim ┐
        desktop (loopback / helper)  ─▶ ring ─▶ auto-level ─▶ trim ─▶ duck (by mic VAD) ───────┴▶ sum
        ─▶ limiter (−1 dBFS) ─▶ VU meters ─▶ local pipe per output ─▶ ffmpeg -f f32le
        (48 kHz f32 stereo, 10 ms blocks, wall-clock paced; Windows: owner-only named pipe,
         elsewhere: Unix socket in a private 0700 temp dir)
         (record file)   tee ─┬─ fifo ─▶ Kick (RTMPS)
                              ├─ fifo ─▶ Rumble (RTMP)
                              └─ fifo ─▶ … (≤ 8, onfail=ignore)
```
- **UI thread** owns all state (`AppState`). Widget callbacks only push `Msg` values into an `Rc<RefCell<VecDeque>>` queue, which is drained after every `app.wait()`. No `RefCell` borrow is held across modal dialogs, and no FLTK awake ring buffer is involved.
- **Sources** are keyed by a *signature* (kind + size + FPS + ffmpeg path). Scenes that share an identical source reuse one capture process. Moves and opacity changes never restart a capture; resizes do.
- **Timing**: all loops pace against an absolute schedule (drift-free) and resync after more than 3 periods of lag. Encoders use `-use_wallclock_as_timestamps 1` with CFR output, which keeps A/V in sync even when a frame write stalls.
- **Zero-copy-ish**: frames move as `Arc<Vec<u8>>`. Reader and canvas buffers are recycled through `Arc::try_unwrap` and a small pool, so there is no 8 MB allocation per frame.

### Tests
```bash
DISPLAY=:0 cargo test --release   # 16 unit tests: blending, clipping, sanitising, args, redaction, destinations, migration, pre-flight, audio transport (incl. L/R alignment, NaN guard), DSP (HPF, gate, compressor, auto-level balance, ducking, limiter), v1.3.1 hardening, v1.3.2 efficiency (chunk reuse, drop reporting), v1.3.3 theme (whitelist, old-profile default, palette switch)
```
(One test builds a default profile, which queries monitor geometry, so it needs an X display; Xvfb works.)

End-to-end audio check (test-only `lavfi` backend, enabled by `LITECAST_TEST_AUDIO=1`; `LITECAST_LOG=1` echoes the log to stderr): set `"audio_api":"lavfi"`, mic `aevalsrc=0.02*sin(2*PI*300*t):s=48000` and desktop `aevalsrc=0.9*sin(2*PI*2000*t):s=48000` (33 dB apart), then `--record 14`. In the recording both tones measure about −20 dB. CI runs this as the **audio balance** step.

### CI & releases
`.github/workflows/ci.yml` runs on every push and pull request:
- **Linux** (Ubuntu 24.04): `cargo fmt --check`, `clippy -D warnings`, unit tests under Xvfb, a release build, and a **headless smoke test**. The smoke test records 4 s of a display + media + colour scene with `--record` and fails if the file has fewer than 60 frames. A **multistream smoke test** then runs `--stream` against two local RTMP listeners and checks that both received H.264 + AAC. An **audio balance** test records synthetic mic and desktop tones 33 dB apart and fails unless both land within 3 dB of each other, near the −20 dB target.
- **Windows** (latest): clippy and release build, which compiles the WASAPI and named-pipe code.
- Artifacts are `litecast_v<ver>-linux-x86_64.tar.gz` and `litecast_v<ver>-windows-x86_64.zip`, each with a SHA-256 file. Download them from the run page.
- **Release**: bump `version` in `Cargo.toml` and the version in this README's title, commit, then
  `git tag v1.3.3 && git push origin v1.3.3`. The tag must match `Cargo.toml`, otherwise CI fails, and the binaries are attached to a GitHub release automatically.

### Security model
- Every child process is spawned with an argv list; no shell is ever invoked.
- Local paths are passed as `file:<path>`, so a filename can't be read as an ffmpeg protocol (`concat:`, `subfile,`, etc.).
- Profiles are size-capped (4 MiB), clamped and repaired by `sanitize()`. Encoder, format and backend names are whitelisted. A broken profile is moved aside to `profile.json.bad`.
- Recordings and screenshots are always written as local files (`file:` protocol), whatever `rec_dir` contains, so a crafted profile cannot upload them to a URL.
- **Loading a profile** (Profile → Load) that names a different ffmpeg program asks first, because LiteCast runs that file. Decline and the current ffmpeg is kept.
- A profile that cannot be *read* (locked, permissions) stops the app with an error instead of being renamed to `.bad`; only invalid or oversized profiles are moved aside.
- The profile is written atomically with mode **0600** in a 0700 directory, because it contains stream keys.
- Destination URLs are validated: scheme whitelist (`rtmp`, `rtmps`, `srt`), length caps, and rejection of `| [ ] \\ ' " < > ` { } ^` and whitespace. A crafted key cannot inject a second tee output or ffmpeg options.
- Stream keys of every destination, URL credentials and SRT passphrases/streamids are **redacted** from the log, the UI and status text.
- Mixed audio reaches ffmpeg over a private local channel. On Unix it is a socket in a fresh 0700 temp dir that accepts one connection and times out after 20 s.
- The `lavfi` test backend (synthetic tones) is refused unless `LITECAST_TEST_AUDIO` is set, so a crafted profile cannot use it to run arbitrary lavfi graphs.
- Windows native audio travels over a named pipe with a random 128-bit name. It has an owner-only DACL (plus SYSTEM), rejects remote clients, and allows exactly one instance (`FILE_FLAG_FIRST_PIPE_INSTANCE`), so other users cannot connect and listen to the microphone.
- **Known limitation**: the full stream URL, including the key, appears in the ffmpeg command line, which other local users can read via `ps` or `/proc`. ffmpeg offers no way to read the output URL from stdin. Use a single-user machine for streaming.

### Limits / not yet implemented
Scene transitions (cut only), text sources, filters (chroma key, crop, colour correction), per-*source* audio (media-file audio, more than one mic), EQ, studio mode, recording splitting, native Wayland/PipeWire capture.

---

## 7. Changelog
- **1.3.3**: dark theme by default, with a new **Settings → Theme** choice: Dark, Light (the pre-1.3.3 look) or System (Windows "app mode", macOS appearance, or GNOME colour scheme; falls back to Dark if the desktop does not say). The change applies live. Fixed the **Log** panel, which had been squeezed to a sliver since 1.3.0: the bottom columns are narrower and log lines now wrap. Section headers are quieter, and the idle Start Recording / Start Streaming buttons are tinted so they stand out. Existing profiles open in Dark; choose Light to keep the old look.
- **1.3.2**: efficiency release. An empty scene no longer composites every frame: about 33% → 14% of a core at 1080p60. Previews are built only after the window drew the previous one, which roughly halves preview work at 60 fps and stops it while the window is minimised. The audio mixer wakes once per 10 ms block instead of every 3 ms, and audio buffers are reused instead of allocated about 100 times a second. If an encoder falls behind and audio is dropped, the log says so and the status bar shows `⚠ audio −N ms`. Log times are now your local time.
- **1.3.1**: audit and bug-fix release. Fixes a left/right channel swap that could start after an audio underrun, NaN/inf samples permanently silencing the mix, CPU spikes from denormal floats during silence, and a stop that could hang when the encoder stopped reading audio. Windows `default` devices now follow a change of the default speakers/mic within about 2 s. Recordings and screenshots are always local files. Loading a profile asks before running a different ffmpeg. Stream keys can no longer leak partly at the log line limit. Unreadable profiles are no longer renamed to `.bad`. Negative X11 capture offsets are clamped.
- **1.3.0**: **audio processing and VU meters**. Mic 80 Hz high-pass, noise gate and soft-knee compressor. **Auto-level** of mic and desktop to one loudness target, so neither is louder or quieter than the other. Optional ducking of desktop under voice. Master −1 dBFS limiter. Live Mic/Desktop/Master meters with peak hold, gain-reduction and auto-gain readouts. **Audio Mixer** dialog (Ctrl+M) with live preview. All backends now use the in-app mixer, so pulse/alsa/dshow no longer lose level to `amix`. New `LITECAST_LOG=1` echoes the log to stderr.
- **1.2.0**: **native Windows audio**. Mic and system audio (WASAPI loopback) are captured by LiteCast, mixed in-app, and fed to ffmpeg over a secure named pipe. This is on by default and needs no Stereo Mix. Silence is filled when nothing plays. Devices reopen automatically after being unplugged. Record and stream share one capture. Old empty-dshow Windows profiles migrate automatically.
- **1.1.1**: fix recording and streaming on current ffmpeg builds (e.g. 2026 Windows builds), which failed with *Error parsing options for input file pipe:0*. `-thread_queue_size` is now passed only to ffmpeg 4.x–6.x.
- **1.1.0**: Kick and Rumble destinations, plus Twitch/YouTube/custom presets. Multistream from one encode (tee + fifo) with per-destination reconnect and status. Silent audio track when no audio is configured. CBR for live x264. RTMPS capability check and pre-flight warnings. `--stream N` CLI flag. v1.0 single URL/key profiles migrate automatically. Rust 1.88+.
- **1.0.0**: initial release.
