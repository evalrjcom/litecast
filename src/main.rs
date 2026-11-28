// =============================================================================
//  litecast_v1.3.3 — LiteCast Studio
//  A lightweight OBS-style recorder / live streamer in Rust + FLTK.
//
//  Architecture (one process, ffmpeg used as an external worker binary):
//
//    [ffmpeg capture proc] --raw BGRA--> reader thread --> FrameSlot ─┐
//    [ffmpeg capture proc] --raw BGRA--> reader thread --> FrameSlot ─┤
//    [color / image source] ---------------------------> FrameSlot ─┤
//                                                                     v
//                        compositor thread (fixed FPS, alpha blend, z-order)
//                           │                     │
//                 canvas slot (Arc)          preview buffer (RGB, downscaled)
//                    │        │                     │
//       writer thread    writer thread         FLTK preview widget
//            │                │
//   [ffmpeg encoder]   [ffmpeg encoder]
//     record file        RTMP/SRT stream
//            ▲                ▲
//   audio: mic/desktop capture ─▶ rings ─▶ mixer thread (HPF, gate, comp,
//          auto-level, ducking, limiter, VU meters) ─▶ local pipe per output
//
//  Build:   cargo build --release      (needs cmake + X11 dev libs on Linux)
//  Run:     ./litecast [--profile FILE] [--record SECONDS] [--stream SECONDS] [--version]
//  Runtime: ffmpeg >= 4.4 on PATH (or set in Settings).
//
//  Changelog
//   1.3.3  UI: dark theme by default, with Settings → Theme (Dark / Light /
//          System; System follows the Windows, macOS or GNOME preference).
//          Fixed the Log column being squeezed to ~45 px since 1.3.0 (dock
//          columns narrowed; log text now wraps). Muted section headers and
//          tinted idle Record/Stream buttons. Search "v1.3.3" in the source.
//   1.3.2  Efficiency: empty scenes skip compositing (re-publish one black
//          frame); previews are built only after the UI drew the last one
//          (~half the preview work at 60 fps, none while minimised); tall
//          previews repeat rows by memcpy; audio mixer sleeps to the next
//          block (300 -> 100 wakeups/s); PCM chunks are recycled instead of
//          allocated per block. Dropped audio (encoder > 1 s behind) is now
//          logged and shown in the status bar. Log times are local time.
//          Search "v1.3.2" in the source.
//   1.3.1  Audit fixes: L/R channel swap after an audio underrun; NaN/inf
//          samples could permanently silence the mix; denormal CPU spikes in
//          silence; Windows "default" device now follows device switches;
//          stop could hang if the encoder stopped reading audio; recordings
//          and screenshots are always local files (no URL upload via
//          rec_dir); loading a profile asks before running a different
//          ffmpeg program; partial stream-key leak at the log line limit;
//          unreadable profiles are no longer renamed to .bad; x11grab
//          negative offsets. Search "v1.3.1 FIX" in the source.
//   1.3.0  Audio processing + VU meters for every backend: mic high-pass,
//          noise gate and soft-knee compressor; auto-level of mic and
//          desktop to one loudness target (so neither is louder); optional
//          desktop ducking under voice; master −1 dBFS limiter; live
//          Mic/Desktop/Master meters (RMS, peak, hold, gain reduction,
//          auto-gain) and an Audio Mixer dialog (Ctrl+M). All backends now
//          feed ffmpeg through the in-app mixer (amix no longer used).
//   1.2.0  Native Windows audio: WASAPI mic + system-audio loopback captured
//          in-app, mixed (silence-filled, wall-clock paced) and piped to
//          ffmpeg over an owner-only named pipe; default on, no Stereo Mix
//          needed. Empty-dshow Windows profiles migrate automatically.
//   1.1.1  Fix: recording/streaming failed on current ffmpeg builds (e.g.
//          Windows gyan/BtbN 2026) with "Error parsing options for input
//          file pipe:0": -thread_queue_size is now output-only there, so
//          it is only passed to ffmpeg 4.x-6.x. Date-versioned git builds
//          are detected as newest.
//   1.1.0  Kick + Rumble (+ Twitch/YouTube/custom) stream destinations with
//          presets; multistream to all enabled destinations from ONE encode
//          (tee + fifo) with per-destination auto-reconnect; silent audio
//          track when no audio device is set; x264 CBR (nal-hrd) for live;
//          RTMPS capability check; pre-flight bitrate/keyframe warnings;
//          v1.0 single URL/key profiles migrate automatically.
//   1.0.0  Initial release: scenes, display/webcam/media/image/color sources,
//          z-order, opacity, drag-move / shift-drag-resize in preview,
//          recording (mkv/mp4/mov/flv/ts), RTMP/RTMPS/SRT streaming with
//          auto-reconnect, mic + desktop audio, HW encoders, screenshots,
//          JSON profiles (0600), headless --record mode.
// =============================================================================
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use fltk::{
    app,
    browser::HoldBrowser,
    button::{Button, CheckButton},
    dialog, draw,
    enums::{Align, Color, ColorDepth, Event, Font, FrameType, Key, Shortcut},
    frame::Frame,
    group::Flex,
    image::RgbImage,
    input::{Input, IntInput, SecretInput},
    menu::{Choice, MenuBar, MenuFlag},
    prelude::*,
    text::{TextBuffer, TextDisplay},
    valuator::HorValueSlider,
    window::Window,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, Command, Stdio},
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// ----------------------------------------------------------------------------
// Constants & limits (hard caps keep a hostile/corrupt profile from exhausting RAM)
// ----------------------------------------------------------------------------
const APP_NAME: &str = "LiteCast Studio";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION"); // single source of truth: Cargo.toml
const MAX_W: u32 = 7680;
const MAX_H: u32 = 4320;
const MIN_DIM: u32 = 2;
const MAX_FPS: u32 = 120;
const MAX_SCENES: usize = 64;
const MAX_SOURCES: usize = 64;
const MAX_PROFILE_BYTES: u64 = 4 << 20;
const LOG_CAP: usize = 2000;
const LOG_VIEW_MAX_BYTES: i32 = 256 * 1024;
const STREAM_MAX_RETRIES: u32 = 6;

const ENCODERS: &[&str] = &[
    "libx264",
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    "h264_vaapi",
    "h264_videotoolbox",
];
const X264_PRESETS: &[&str] = &[
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
];
const REC_FORMATS: &[&str] = &["mkv", "mp4", "mov", "flv", "ts"];
/// v1.3.3: UI themes (Settings → Theme).
const THEMES: &[&str] = &["dark", "light", "system"];
const AUDIO_APIS: &[&str] = &["none", "wasapi", "pulse", "alsa", "dshow", "avfoundation"];
/// Backends offered on this OS (wasapi = native Windows capture).
fn audio_apis_here() -> Vec<&'static str> {
    AUDIO_APIS
        .iter()
        .copied()
        .filter(|a| cfg!(windows) || *a != "wasapi")
        .collect()
}

/// Internal pixel format. BGRA matches x11grab/gdigrab native order, so the
/// capture side does no channel swizzle (only alpha fill).
const PIX: &str = "bgra";

type FrameSlot = Arc<Mutex<Option<Arc<Vec<u8>>>>>;

/// Poison-tolerant lock: a panicking worker must never take the UI down.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn sleep_interruptible(d: Duration, stop: &AtomicBool) {
    let end = Instant::now() + d;
    while !stop.load(Relaxed) {
        let now = Instant::now();
        if now >= end {
            break;
        }
        thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn even(v: u32) -> u32 {
    v & !1
}

/// v1.3.2: current local time of day (h, m, s), for log stamps.
#[cfg(unix)]
fn local_hms() -> (u32, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = secs as libc::time_t;
    // SAFETY: localtime_r is the thread-safe variant; both pointers are valid
    // for the call and `tm` is plain data (all-zero is a valid value).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return (
            (secs / 3600 % 24) as u32,
            (secs / 60 % 60) as u32,
            (secs % 60) as u32,
        );
    }
    (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32)
}

#[cfg(windows)]
fn local_hms() -> (u32, u32, u32) {
    // SAFETY: GetLocalTime only writes the SYSTEMTIME we pass.
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st) };
    (st.wHour as u32, st.wMinute as u32, st.wSecond as u32)
}

// ----------------------------------------------------------------------------
// Logger (thread-safe ring buffer with secret redaction)
// ----------------------------------------------------------------------------
#[derive(Clone, Default)]
struct Logger {
    inner: Arc<Mutex<LogInner>>,
}
#[derive(Default)]
struct LogInner {
    lines: VecDeque<(u64, String)>,
    seq: u64,
    secrets: Vec<String>,
}
impl Logger {
    fn log(&self, msg: impl AsRef<str>) {
        let mut g = lock(&self.inner);
        // Redact stream keys / credentials: ffmpeg echoes the output URL in errors.
        // v1.3.1 FIX: redact *before* truncating. Truncating first could cut a
        // key in half at the 600-char limit, and the surviving prefix no
        // longer matched, so part of the key was logged.
        let mut line = msg.as_ref().to_string();
        for s in &g.secrets {
            if s.len() >= 4 && line.contains(s.as_str()) {
                line = line.replace(s.as_str(), "****");
            }
        }
        let line = truncate_chars(&line, 600);
        // v1.3.2: local wall-clock time (was UTC with no marker, which read
        // as the wrong time of day everywhere outside UTC).
        let (h, m, sec) = local_hms();
        let line = format!("{h:02}:{m:02}:{sec:02} {line}");
        // Echo to stderr in debug builds, or when LITECAST_LOG=1 (CI / headless runs).
        if cfg!(debug_assertions) || std::env::var_os("LITECAST_LOG").is_some() {
            eprintln!("{line}");
        }
        g.seq += 1;
        let seq = g.seq;
        g.lines.push_back((seq, line));
        while g.lines.len() > LOG_CAP {
            g.lines.pop_front();
        }
    }
    fn set_secrets(&self, v: Vec<String>) {
        lock(&self.inner).secrets = v.into_iter().filter(|s| s.len() >= 4).collect();
    }
    fn since(&self, seq: u64) -> (u64, Vec<String>) {
        let g = lock(&self.inner);
        let v = g
            .lines
            .iter()
            .filter(|(s, _)| *s > seq)
            .map(|(_, l)| l.clone())
            .collect();
        (g.seq, v)
    }
}

// ----------------------------------------------------------------------------
// Profile model (serde)
// ----------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
struct Settings {
    canvas_w: u32,
    canvas_h: u32,
    fps: u32,
    encoder: String,
    x264_preset: String,
    video_kbps: u32,
    audio_kbps: u32,
    keyint_sec: u32,
    rec_dir: String,
    rec_format: String,
    /// v1.1: multistream destinations (Kick, Rumble, Twitch, YouTube, custom).
    destinations: Vec<Destination>,
    /// v1.0 single-destination fields: read for migration, never written back.
    #[serde(skip_serializing)]
    stream_url: String,
    #[serde(skip_serializing)]
    stream_key: String,
    audio_api: String,
    mic_device: String,
    desktop_device: String,
    /// v1.3: mic compressor/gate, auto-level, ducking, limiter.
    audio_dsp: AudioDsp,
    /// v1.3.3: "dark" (default), "light" (classic) or "system".
    theme: String,
    ffmpeg_path: String,
    vaapi_device: String,
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Default for Settings {
    fn default() -> Self {
        let videos = home_dir().join("Videos");
        let rec_dir = if videos.is_dir() { videos } else { home_dir() };
        let (api, mic, desk) = if cfg!(windows) {
            // Native WASAPI: default mic + system-audio loopback, no setup.
            ("wasapi", "default", "default")
        } else if cfg!(target_os = "macos") {
            ("avfoundation", "", "")
        } else {
            ("pulse", "default", "@DEFAULT_MONITOR@")
        };
        Self {
            canvas_w: 1920,
            canvas_h: 1080,
            fps: 30,
            encoder: "libx264".into(),
            x264_preset: "veryfast".into(),
            video_kbps: 4500,
            audio_kbps: 160,
            keyint_sec: 2,
            rec_dir: rec_dir.to_string_lossy().into_owned(),
            rec_format: "mkv".into(),
            destinations: vec![],
            stream_url: String::new(),
            stream_key: String::new(),
            audio_api: api.into(),
            mic_device: mic.into(),
            desktop_device: desk.into(),
            theme: "dark".into(),
            audio_dsp: AudioDsp::default(),
            ffmpeg_path: "ffmpeg".into(),
            vaapi_device: "/dev/dri/renderD128".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SourceKind {
    Display {
        target: String,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        cursor: bool,
    },
    Webcam {
        device: String,
        size: String,
        fps: u32,
    },
    Media {
        path: String,
        looping: bool,
    },
    Image {
        path: String,
    },
    Color {
        r: u8,
        g: u8,
        b: u8,
    },
}
impl SourceKind {
    fn label(&self) -> &'static str {
        match self {
            SourceKind::Display { .. } => "Display",
            SourceKind::Webcam { .. } => "Webcam",
            SourceKind::Media { .. } => "Media",
            SourceKind::Image { .. } => "Image",
            SourceKind::Color { .. } => "Color",
        }
    }
    fn index(&self) -> i32 {
        match self {
            SourceKind::Display { .. } => 0,
            SourceKind::Webcam { .. } => 1,
            SourceKind::Media { .. } => 2,
            SourceKind::Image { .. } => 3,
            SourceKind::Color { .. } => 4,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Source {
    id: u64,
    name: String,
    kind: SourceKind,
    visible: bool,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    opacity: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Scene {
    name: String,
    sources: Vec<Source>, // index 0 = bottom layer
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
struct Profile {
    format: u32,
    settings: Settings,
    scenes: Vec<Scene>,
    current: usize,
}
impl Default for Profile {
    fn default() -> Self {
        let s = Settings::default();
        let (sx, sy, sw, sh) = primary_screen_px();
        let (fx, fy, fw, fh) = fit_rect(
            sw.max(2) as u32,
            sh.max(2) as u32,
            s.canvas_w as i32,
            s.canvas_h as i32,
        );
        let disp = Source {
            id: 1,
            name: "Display Capture".into(),
            kind: SourceKind::Display {
                target: default_display_target(),
                x: sx,
                y: sy,
                w: sw.max(2) as u32,
                h: sh.max(2) as u32,
                cursor: true,
            },
            visible: true,
            x: fx,
            y: fy,
            w: fw.max(2) as u32,
            h: fh.max(2) as u32,
            opacity: 255,
        };
        Self {
            format: 1,
            settings: s,
            scenes: vec![Scene {
                name: "Scene 1".into(),
                sources: vec![disp],
            }],
            current: 0,
        }
    }
}

fn default_display_target() -> String {
    if cfg!(windows) {
        "desktop".into()
    } else if cfg!(target_os = "macos") {
        "1".into()
    } else {
        std::env::var("DISPLAY")
            .ok()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| ":0.0".into())
    }
}
fn default_webcam_target() -> String {
    if cfg!(windows) {
        String::new()
    } else if cfg!(target_os = "macos") {
        "0".into()
    } else {
        "/dev/video0".into()
    }
}

/// FLTK reports logical coords; capture APIs want physical pixels.
fn screens_px() -> Vec<(i32, i32, i32, i32)> {
    let n = app::screen_count().max(0);
    (0..n)
        .map(|i| {
            let (x, y, w, h) = app::screen_xywh(i);
            let s = app::screen_scale(i).max(0.1) as f64;
            let f = |v: i32| (v as f64 * s).round() as i32;
            (f(x), f(y), f(w), f(h))
        })
        .collect()
}
fn primary_screen_px() -> (i32, i32, i32, i32) {
    screens_px().first().copied().unwrap_or((0, 0, 1920, 1080))
}

/// Clamp/repair everything coming from disk. Never trust a profile file.
fn sanitize(p: &mut Profile) {
    let d = Settings::default();
    let s = &mut p.settings;
    s.canvas_w = even(s.canvas_w.clamp(160, MAX_W));
    s.canvas_h = even(s.canvas_h.clamp(120, MAX_H));
    s.fps = s.fps.clamp(1, MAX_FPS);
    s.video_kbps = s.video_kbps.clamp(100, 200_000);
    s.audio_kbps = s.audio_kbps.clamp(32, 512);
    s.keyint_sec = s.keyint_sec.clamp(1, 20);
    if !ENCODERS.contains(&s.encoder.as_str()) {
        s.encoder = d.encoder.clone();
    }
    if !X264_PRESETS.contains(&s.x264_preset.as_str()) {
        s.x264_preset = d.x264_preset.clone();
    }
    if !REC_FORMATS.contains(&s.rec_format.as_str()) {
        s.rec_format = d.rec_format.clone();
    }
    if !THEMES.contains(&s.theme.as_str()) {
        s.theme = d.theme.clone(); // v1.3.3: whitelist
    }
    migrate_audio(s, cfg!(windows), &d);
    s.audio_dsp.sanitize();
    if s.ffmpeg_path.trim().is_empty() {
        s.ffmpeg_path = d.ffmpeg_path.clone();
    }
    for f in [
        &mut s.rec_dir,
        &mut s.stream_url,
        &mut s.stream_key,
        &mut s.mic_device,
        &mut s.desktop_device,
        &mut s.ffmpeg_path,
        &mut s.vaapi_device,
    ] {
        *f = truncate_chars(&f.replace('\0', ""), 1024);
    }
    // v1.0 -> v1.1: the single URL/key pair becomes a destination.
    if s.destinations.is_empty() && !s.stream_key.trim().is_empty() {
        let svc = guess_service(&s.stream_url);
        s.destinations.push(Destination {
            name: service_info(svc).label.into(),
            service: svc.into(),
            url: s.stream_url.trim().into(),
            key: s.stream_key.trim().into(),
            enabled: true,
        });
    }
    s.stream_url.clear();
    s.stream_key.clear();
    s.destinations.truncate(MAX_DESTINATIONS);
    for d in &mut s.destinations {
        if !SERVICES.iter().any(|x| x.id == d.service) {
            d.service = "custom".into();
        }
        d.name = truncate_chars(d.name.replace('\0', "").trim(), 48);
        if d.name.is_empty() {
            d.name = service_info(&d.service).label.into();
        }
        d.url = truncate_chars(d.url.replace('\0', "").trim(), 1024);
        d.key = truncate_chars(d.key.replace('\0', "").trim(), 512);
    }
    p.scenes.truncate(MAX_SCENES);
    if p.scenes.is_empty() {
        p.scenes.push(Scene {
            name: "Scene 1".into(),
            sources: vec![],
        });
    }
    let mut seen = HashSet::new();
    let mut next_id = p
        .scenes
        .iter()
        .flat_map(|s| s.sources.iter().map(|x| x.id))
        .max()
        .unwrap_or(0)
        + 1;
    for sc in &mut p.scenes {
        sc.name = truncate_chars(&sc.name, 64);
        sc.sources.truncate(MAX_SOURCES);
        for src in &mut sc.sources {
            if src.id == 0 || !seen.insert(src.id) {
                src.id = next_id;
                seen.insert(next_id);
                next_id += 1;
            }
            src.name = truncate_chars(&src.name, 64);
            src.w = src.w.clamp(MIN_DIM, MAX_W);
            src.h = src.h.clamp(MIN_DIM, MAX_H);
            src.x = src.x.clamp(-(MAX_W as i32), MAX_W as i32);
            src.y = src.y.clamp(-(MAX_H as i32), MAX_H as i32);
            match &mut src.kind {
                SourceKind::Display {
                    w, h, x, y, target, ..
                } => {
                    *w = (*w).clamp(1, 16384);
                    *h = (*h).clamp(1, 16384);
                    *x = (*x).clamp(-32768, 32767);
                    *y = (*y).clamp(-32768, 32767);
                    *target = truncate_chars(&target.replace('\0', ""), 512);
                }
                SourceKind::Webcam { device, size, fps } => {
                    *device = truncate_chars(&device.replace('\0', ""), 512);
                    if parse_size(size).is_none() {
                        size.clear();
                    }
                    *fps = (*fps).min(MAX_FPS);
                }
                SourceKind::Media { path, .. } | SourceKind::Image { path } => {
                    *path = truncate_chars(&path.replace('\0', ""), 4096);
                }
                SourceKind::Color { .. } => {}
            }
        }
    }
    if p.current >= p.scenes.len() {
        p.current = 0;
    }
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (a, b) = s.split_once(['x', 'X'])?;
    let w: u32 = a.trim().parse().ok()?;
    let h: u32 = b.trim().parse().ok()?;
    ((1..=MAX_W).contains(&w) && (1..=MAX_H).contains(&h)).then_some((w, h))
}

fn config_dir() -> PathBuf {
    if cfg!(windows)
        && let Some(a) = std::env::var_os("APPDATA")
    {
        return PathBuf::from(a).join("litecast");
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(x).join("litecast");
    }
    home_dir().join(".config").join("litecast")
}

fn load_profile(path: &Path) -> Result<Profile, String> {
    let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_PROFILE_BYTES {
        return Err("profile too large (>4 MiB)".into());
    }
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let mut p: Profile =
        serde_json::from_slice(&data).map_err(|e| format!("invalid profile: {e}"))?;
    sanitize(&mut p);
    Ok(p)
}

/// Atomic write (tmp + rename) with 0600 perms — the profile holds the stream key.
fn save_profile(path: &Path, p: &Profile) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    #[cfg(not(unix))]
    {
        let _ = fs::create_dir_all(dir);
    }
    let data = serde_json::to_vec_pretty(p).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut o = fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(&data).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

// ----------------------------------------------------------------------------
// ffmpeg helpers
// ----------------------------------------------------------------------------
fn ffmpeg_cmd(prog: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(prog);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// v1.3.1: doc comment fixed (its first two lines belonged to file_arg).
/// Unknown/foreign backends fall back to the platform default. On Windows,
/// a pre-1.2 dshow profile with no devices (which recorded *no audio at
/// all*) is upgraded to native mic + system audio.
fn migrate_audio(s: &mut Settings, windows: bool, d: &Settings) {
    // "lavfi" = synthetic test source for CI, only with LITECAST_TEST_AUDIO=1.
    let test = s.audio_api == "lavfi" && std::env::var_os("LITECAST_TEST_AUDIO").is_some();
    let ok = test
        || (AUDIO_APIS.contains(&s.audio_api.as_str()) && (windows || s.audio_api != "wasapi"));
    let empty_dshow = windows
        && s.audio_api == "dshow"
        && s.mic_device.trim().is_empty()
        && s.desktop_device.trim().is_empty();
    if !ok || empty_dshow {
        s.audio_api = d.audio_api.clone();
        s.mic_device = d.mic_device.clone();
        s.desktop_device = d.desktop_device.clone();
    }
}

/// v1.3.1: for files LiteCast *writes* (recordings, screenshots): always the
/// file protocol, even if the path looks like a URL.
fn local_file_arg(p: &str) -> String {
    format!("file:{p}")
}

/// Inputs: force the file protocol for local paths so names like "concat:..."
/// or "subfile,..." cannot be read as ffmpeg protocols. "scheme://" inputs
/// (network media) are passed through on purpose.
fn file_arg(p: &str) -> String {
    if p.contains("://") {
        p.to_string()
    } else {
        format!("file:{p}")
    }
}

#[derive(Clone, Debug, Default)]
struct FfInfo {
    ok: bool,
    major: u32,
    minor: u32,
    version: String,
    encoders: HashSet<String>,
    out_protocols: HashSet<String>,
}
impl FfInfo {
    fn has_fps_mode(&self) -> bool {
        self.major > 5 || (self.major == 5 && self.minor >= 1)
    }
    /// `-thread_queue_size` as an *input* option: effective in 4.x-6.x,
    /// silently ignored in 7.x, and rejected outright by current builds
    /// (now output/muxer-only), which aborts the encoder. Emit it only
    /// where it still helps device inputs.
    fn input_queue_opt(&self) -> bool {
        (4..=6).contains(&self.major)
    }
}

/// Run a short-lived tool with bounded output, optional stdin, timeout and
/// cooperative cancellation. Never blocks forever.
fn run_tool(
    prog: &str,
    args: &[String],
    stdin_data: Option<Arc<Vec<u8>>>,
    max_out: u64,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<(Vec<u8>, String, Option<i32>), String> {
    let mut cmd = ffmpeg_cmd(prog);
    cmd.args(args)
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {prog}: {e}"))?;
    let writer = match (stdin_data, child.stdin.take()) {
        (Some(d), Some(mut si)) => Some(thread::spawn(move || {
            let _ = si.write_all(&d);
        })),
        _ => None,
    };
    let out = child.stdout.take().ok_or("no stdout")?;
    let err = child.stderr.take().ok_or("no stderr")?;
    let to = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.take(max_out).read_to_end(&mut v);
        v
    });
    let te = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.take(256 * 1024).read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + timeout;
    let mut code = None;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                code = st.code();
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline || cancel.map(|c| c.load(Relaxed)).unwrap_or(false) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    if let Some(w) = writer {
        let _ = w.join();
    }
    let o = to.join().unwrap_or_default();
    let e = te.join().unwrap_or_default();
    Ok((o, String::from_utf8_lossy(&e).into_owned(), code))
}

fn detect_ffmpeg(prog: &str) -> FfInfo {
    let mut fi = FfInfo::default();
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if let Ok((o, _, Some(0))) = run_tool(
        prog,
        &a(&["-hide_banner", "-version"]),
        None,
        64 * 1024,
        Duration::from_secs(5),
        None,
    ) {
        let s = String::from_utf8_lossy(&o);
        let first = s.lines().next().unwrap_or("").to_string();
        fi.version = first.clone();
        let ver = first
            .split_whitespace()
            .nth(2)
            .unwrap_or("")
            .trim_start_matches('n');
        let mut it = ver.split(|c: char| !c.is_ascii_digit());
        let maj = it.next().and_then(|v| v.parse().ok());
        let min = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        // Git builds ("N-12345-g...") are always newer than any release.
        // Date-versioned git builds (gyan.dev "2026-11-20-git-...") likewise.
        (fi.major, fi.minor) = match maj {
            Some(m) if m < 1000 => (m, min),
            _ => (99, 0),
        };
        fi.ok = true;
    }
    if fi.ok
        && let Ok((o, _, _)) = run_tool(
            prog,
            &a(&["-hide_banner", "-protocols"]),
            None,
            128 * 1024,
            Duration::from_secs(5),
            None,
        )
    {
        let txt = String::from_utf8_lossy(&o);
        if let Some((_, outs)) = txt.split_once("Output:") {
            fi.out_protocols = outs.split_whitespace().map(str::to_string).collect();
        }
    }
    if fi.ok
        && let Ok((o, _, _)) = run_tool(
            prog,
            &a(&["-hide_banner", "-encoders"]),
            None,
            512 * 1024,
            Duration::from_secs(5),
            None,
        )
    {
        for line in String::from_utf8_lossy(&o).lines() {
            let mut t = line.split_whitespace();
            if let (Some(flags), Some(name)) = (t.next(), t.next())
                && flags.len() == 6
                && (flags.starts_with('V') || flags.starts_with('A'))
            {
                fi.encoders.insert(name.to_string());
            }
        }
    }
    fi
}

/// (value, label) pairs. Value is what goes into the ffmpeg -i argument.
type DevList = Vec<(String, String)>;

fn parse_dshow(ff: &str) -> (DevList, DevList) {
    let args: Vec<String> = [
        "-hide_banner",
        "-list_devices",
        "true",
        "-f",
        "dshow",
        "-i",
        "dummy",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let (mut v, mut a) = (vec![], vec![]);
    if let Ok((_, err, _)) = run_tool(ff, &args, None, 1024, Duration::from_secs(6), None) {
        let mut section = 0; // 1 video, 2 audio
        for line in err.lines() {
            if line.contains("DirectShow video devices") {
                section = 1;
                continue;
            }
            if line.contains("DirectShow audio devices") {
                section = 2;
                continue;
            }
            if line.contains("Alternative name") {
                continue;
            }
            let (Some(s), Some(e)) = (line.find('"'), line.rfind('"')) else {
                continue;
            };
            if e <= s + 1 {
                continue;
            }
            let name = line[s + 1..e].to_string();
            let kind = if line.ends_with("(video)") {
                1
            } else if line.ends_with("(audio)") {
                2
            } else {
                section
            };
            match kind {
                1 => v.push((name.clone(), name)),
                2 => a.push((name.clone(), name)),
                _ => {}
            }
        }
    }
    (v, a)
}

fn parse_avfoundation(ff: &str) -> (DevList, DevList) {
    let args: Vec<String> = [
        "-hide_banner",
        "-f",
        "avfoundation",
        "-list_devices",
        "true",
        "-i",
        "",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let (mut v, mut a) = (vec![], vec![]);
    if let Ok((_, err, _)) = run_tool(ff, &args, None, 1024, Duration::from_secs(6), None) {
        let mut section = 0;
        for line in err.lines() {
            if line.contains("video devices:") {
                section = 1;
                continue;
            }
            if line.contains("audio devices:") {
                section = 2;
                continue;
            }
            // "...] [0] FaceTime HD Camera"
            if let Some(p) = line.rfind("] [") {
                let rest = &line[p + 3..];
                if let Some(q) = rest.find(']') {
                    let idx = rest[..q].to_string();
                    let label = format!("{} — {}", idx, rest[q + 1..].trim());
                    if idx.chars().all(|c| c.is_ascii_digit()) {
                        match section {
                            1 => v.push((idx, label)),
                            2 => a.push((idx, label)),
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    (v, a)
}

fn detect_video_devices(ff: &str) -> DevList {
    if cfg!(windows) {
        parse_dshow(ff).0
    } else if cfg!(target_os = "macos") {
        parse_avfoundation(ff).0
    } else {
        let mut v: Vec<String> = fs::read_dir("/dev")
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| {
                        n.starts_with("video") && n[5..].chars().all(|c| c.is_ascii_digit())
                    })
                    .map(|n| format!("/dev/{n}"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|n| n[10..].parse::<u32>().unwrap_or(u32::MAX));
        v.into_iter().map(|n| (n.clone(), n)).collect()
    }
}

fn detect_audio_devices(api: &str, ff: &str) -> DevList {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match api {
        "pulse" => {
            let mut v: DevList = vec![
                ("default".into(), "default (default microphone)".into()),
                (
                    "@DEFAULT_MONITOR@".into(),
                    "@DEFAULT_MONITOR@ (desktop audio)".into(),
                ),
            ];
            if let Ok((o, _, _)) = run_tool(
                "pactl",
                &s(&["list", "short", "sources"]),
                None,
                256 * 1024,
                Duration::from_secs(3),
                None,
            ) {
                for l in String::from_utf8_lossy(&o).lines() {
                    if let Some(n) = l.split('\t').nth(1) {
                        v.push((n.to_string(), n.to_string()));
                    }
                }
            }
            v
        }
        "alsa" => {
            let mut v: DevList = vec![("default".into(), "default".into())];
            if let Ok((o, _, _)) = run_tool(
                "arecord",
                &s(&["-L"]),
                None,
                256 * 1024,
                Duration::from_secs(3),
                None,
            ) {
                for l in String::from_utf8_lossy(&o).lines() {
                    if !l.is_empty()
                        && !l.starts_with(char::is_whitespace)
                        && l != "default"
                        && l != "null"
                    {
                        v.push((l.to_string(), l.to_string()));
                    }
                }
            }
            v
        }
        "wasapi" => wasapi_devices(),
        "dshow" => parse_dshow(ff).1,
        "avfoundation" => parse_avfoundation(ff).1,
        _ => vec![],
    }
}

/// Bounded line reader for child stderr. Drains continuously so a chatty
/// ffmpeg can never fill the pipe and deadlock.
type LineHook = Box<dyn Fn(&str) + Send>;
/// (name, ingest URL, last error time) per stream destination.
type DestHealth = Arc<Mutex<Vec<(String, String, Option<Instant>)>>>;

fn spawn_stderr_drain(
    err: ChildStderr,
    tag: String,
    log: Logger,
    last: Arc<Mutex<String>>,
) -> Option<JoinHandle<()>> {
    spawn_stderr_drain_hook(err, tag, log, last, None)
}

/// Same, with an optional per-line hook (sees the raw line before redaction;
/// used to track per-destination stream health).
fn spawn_stderr_drain_hook(
    err: ChildStderr,
    tag: String,
    log: Logger,
    last: Arc<Mutex<String>>,
    hook: Option<LineHook>,
) -> Option<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("stderr-{tag}"))
        .spawn(move || {
            let mut r = BufReader::with_capacity(8192, err);
            let mut buf = Vec::with_capacity(512);
            loop {
                buf.clear();
                match std::io::BufRead::read_until(&mut (&mut r).take(4096), b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let s = String::from_utf8_lossy(&buf);
                        let s = s.trim();
                        if s.is_empty() {
                            continue;
                        }
                        if let Some(h) = &hook {
                            h(s);
                        }
                        *lock(&last) = truncate_chars(s, 300);
                        log.log(format!("[{tag}] {s}"));
                    }
                }
            }
        })
        .ok()
}

/// Like read_exact but reports a clean EOF at a frame boundary as Ok(false).
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..]) {
            Ok(0) => {
                return if off == 0 {
                    Ok(false)
                } else {
                    Err(std::io::ErrorKind::UnexpectedEof.into())
                };
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn all_opaque(buf: &[u8]) -> bool {
    buf.iter().skip(3).step_by(4).all(|&a| a == 255)
}

/// Swap a new frame into a slot; returns the previous buffer if nobody else
/// holds it, so the reader can recycle the allocation (no 8 MB alloc/frame).
fn publish(slot: &FrameSlot, buf: Vec<u8>) -> Option<Vec<u8>> {
    let old = lock(slot).replace(Arc::new(buf));
    old.and_then(|a| Arc::try_unwrap(a).ok())
}

/// Build the ffmpeg capture command for a live/media source. Output is raw
/// BGRA at exactly the source's transform size.
fn capture_args(kind: &SourceKind, w: u32, h: u32, fps: u32) -> Result<Vec<String>, String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-nostats"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut pre_vf = String::new();
    let p = |a: &mut Vec<String>, v: &[&str]| a.extend(v.iter().map(|s| s.to_string()));
    match kind {
        SourceKind::Display {
            target,
            x,
            y,
            w: rw,
            h: rh,
            cursor,
        } => {
            let cur = if *cursor { "1" } else { "0" };
            let fps_s = fps.to_string();
            let size = format!("{rw}x{rh}");
            if cfg!(windows) {
                p(
                    &mut a,
                    &["-f", "gdigrab", "-framerate", &fps_s, "-draw_mouse", cur],
                );
                if target.starts_with("title=") {
                    p(&mut a, &["-i", target]);
                } else {
                    let (xs, ys) = (x.to_string(), y.to_string());
                    p(
                        &mut a,
                        &[
                            "-offset_x",
                            &xs,
                            "-offset_y",
                            &ys,
                            "-video_size",
                            &size,
                            "-i",
                            "desktop",
                        ],
                    );
                }
            } else if cfg!(target_os = "macos") {
                let t = if target.trim().is_empty() {
                    "1".to_string()
                } else {
                    target.clone()
                };
                let inp = format!("{t}:none");
                p(
                    &mut a,
                    &[
                        "-f",
                        "avfoundation",
                        "-capture_cursor",
                        cur,
                        "-framerate",
                        &fps_s,
                        "-i",
                        &inp,
                    ],
                );
                pre_vf = format!("crop={rw}:{rh}:{x}:{y},");
            } else {
                let disp = if target.trim().is_empty() {
                    default_display_target()
                } else {
                    target.clone()
                };
                // v1.3.1 FIX: x11grab rejects "+-10,0"; X11 screen coords are >= 0.
                let inp = format!("{disp}+{},{}", (*x).max(0), (*y).max(0));
                p(
                    &mut a,
                    &[
                        "-f",
                        "x11grab",
                        "-framerate",
                        &fps_s,
                        "-draw_mouse",
                        cur,
                        "-video_size",
                        &size,
                        "-i",
                        &inp,
                    ],
                );
            }
        }
        SourceKind::Webcam {
            device,
            size,
            fps: wf,
        } => {
            if device.trim().is_empty() {
                return Err("no webcam device set".into());
            }
            let api = if cfg!(windows) {
                "dshow"
            } else if cfg!(target_os = "macos") {
                "avfoundation"
            } else {
                "v4l2"
            };
            p(&mut a, &["-f", api]);
            if cfg!(windows) {
                p(&mut a, &["-rtbufsize", "64M"]);
            }
            if parse_size(size).is_some() {
                p(&mut a, &["-video_size", size.trim()]);
            }
            if *wf > 0 {
                let f = wf.to_string();
                p(&mut a, &["-framerate", &f]);
            }
            let inp = if cfg!(windows) {
                format!("video={device}")
            } else if cfg!(target_os = "macos") {
                format!("{device}:none")
            } else {
                device.clone()
            };
            p(&mut a, &["-i", &inp]);
        }
        SourceKind::Media { path, looping } => {
            if path.trim().is_empty() {
                return Err("no media file set".into());
            }
            if *looping {
                p(&mut a, &["-stream_loop", "-1"]);
            }
            let fa = file_arg(path);
            p(&mut a, &["-re", "-i", &fa]);
        }
        _ => return Err("not a process source".into()),
    }
    let vf = format!("{pre_vf}scale={w}:{h}:flags=bilinear,format={PIX}");
    p(
        &mut a,
        &[
            "-an", "-sn", "-dn", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", PIX, "pipe:1",
        ],
    );
    Ok(a)
}

// ----------------------------------------------------------------------------
// Source runtime (one per distinct source signature; shared across scenes)
// ----------------------------------------------------------------------------
struct SourceRuntime {
    slot: FrameSlot,
    opaque: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
    status: Arc<Mutex<String>>,
}

struct RtCtx {
    ffmpeg: String,
    fps: u32,
    log: Logger,
}

/// Anything that changes the produced pixels must be in the signature.
fn source_sig(s: &Source, fps: u32, ff: &str) -> String {
    format!("{:?}|{}x{}|{}|{}", s.kind, s.w, s.h, fps, ff)
}

impl SourceRuntime {
    fn start(src: &Source, ctx: &RtCtx) -> Self {
        let mut rt = Self {
            slot: Arc::new(Mutex::new(None)),
            opaque: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            thread: None,
            status: Arc::new(Mutex::new("starting".into())),
        };
        let (w, h) = (src.w, src.h);
        let name = src.name.clone();
        match &src.kind {
            SourceKind::Color { r, g, b } => {
                let px = [*b, *g, *r, 255u8];
                let v = px.repeat(w as usize * h as usize);
                *lock(&rt.slot) = Some(Arc::new(v));
                *lock(&rt.status) = "ok".into();
            }
            SourceKind::Image { path } => {
                let args: Vec<String> = vec![
                    "-hide_banner".into(),
                    "-loglevel".into(),
                    "error".into(),
                    "-nostdin".into(),
                    "-i".into(),
                    file_arg(path),
                    "-frames:v".into(),
                    "1".into(),
                    "-vf".into(),
                    format!("scale={w}:{h}:flags=lanczos,format={PIX}"),
                    "-f".into(),
                    "rawvideo".into(),
                    "-pix_fmt".into(),
                    PIX.into(),
                    "pipe:1".into(),
                ];
                let (slot, opaque, stop, status, log, ff) = (
                    rt.slot.clone(),
                    rt.opaque.clone(),
                    rt.stop.clone(),
                    rt.status.clone(),
                    ctx.log.clone(),
                    ctx.ffmpeg.clone(),
                );
                let need = w as usize * h as usize * 4;
                rt.thread = thread::Builder::new()
                    .name(format!("img-{name}"))
                    .spawn(move || {
                        match run_tool(
                            &ff,
                            &args,
                            None,
                            need as u64,
                            Duration::from_secs(20),
                            Some(&stop),
                        ) {
                            Ok((buf, _, _)) if buf.len() == need => {
                                opaque.store(all_opaque(&buf), Relaxed);
                                *lock(&slot) = Some(Arc::new(buf));
                                *lock(&status) = "ok".into();
                            }
                            Ok((_, err, _)) => {
                                let e = truncate_chars(err.trim(), 200);
                                log.log(format!("[{name}] image decode failed: {e}"));
                                *lock(&status) = format!("error: {e}");
                            }
                            Err(e) => {
                                log.log(format!("[{name}] {e}"));
                                *lock(&status) = format!("error: {e}");
                            }
                        }
                    })
                    .ok();
            }
            kind => {
                let args = match capture_args(kind, w, h, ctx.fps) {
                    Ok(a) => a,
                    Err(e) => {
                        *lock(&rt.status) = format!("error: {e}");
                        ctx.log.log(format!("[{name}] {e}"));
                        return rt;
                    }
                };
                let noloop_media = matches!(kind, SourceKind::Media { looping: false, .. });
                let cfg = CapCfg {
                    name: name.clone(),
                    args,
                    frame_len: w as usize * h as usize * 4,
                    ffmpeg: ctx.ffmpeg.clone(),
                    log: ctx.log.clone(),
                    slot: rt.slot.clone(),
                    opaque: rt.opaque.clone(),
                    stop: rt.stop.clone(),
                    child: rt.child.clone(),
                    status: rt.status.clone(),
                    noloop_media,
                };
                rt.thread = thread::Builder::new()
                    .name(format!("cap-{name}"))
                    .spawn(move || capture_loop(cfg))
                    .ok();
            }
        }
        rt
    }

    fn stop(mut self) {
        self.stop.store(true, Relaxed);
        if let Some(c) = lock(&self.child).as_mut() {
            let _ = c.kill(); // unblocks the reader's read()
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct CapCfg {
    name: String,
    args: Vec<String>,
    frame_len: usize,
    ffmpeg: String,
    log: Logger,
    slot: FrameSlot,
    opaque: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    status: Arc<Mutex<String>>,
    noloop_media: bool,
}

/// Supervised capture: spawns ffmpeg, reads fixed-size frames, restarts with
/// exponential backoff on failure (e.g. webcam unplugged, X server hiccup).
fn capture_loop(c: CapCfg) {
    let mut backoff = Duration::from_secs(1);
    let mut spare: Option<Vec<u8>> = None;
    while !c.stop.load(Relaxed) {
        let mut cmd = ffmpeg_cmd(&c.ffmpeg);
        cmd.args(&c.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(ch) => ch,
            Err(e) => {
                *lock(&c.status) = format!("error: cannot start ffmpeg: {e}");
                c.log.log(format!(
                    "[{}] cannot start ffmpeg ({}): {e}",
                    c.name, c.ffmpeg
                ));
                sleep_interruptible(backoff, &c.stop);
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        let stdout = child.stdout.take();
        let last_err = Arc::new(Mutex::new(String::new()));
        let drain = child
            .stderr
            .take()
            .and_then(|e| spawn_stderr_drain(e, c.name.clone(), c.log.clone(), last_err.clone()));
        *lock(&c.child) = Some(child);
        if c.stop.load(Relaxed)
            && let Some(ch) = lock(&c.child).as_mut()
        {
            let _ = ch.kill();
        }
        let started = Instant::now();
        let mut frames: u64 = 0;
        let mut clean_eof = false;
        if let Some(mut out) = stdout {
            loop {
                let mut buf = spare
                    .take()
                    .filter(|b| b.len() == c.frame_len)
                    .unwrap_or_else(|| vec![0u8; c.frame_len]);
                match read_full(&mut out, &mut buf) {
                    Ok(true) => {}
                    Ok(false) => {
                        clean_eof = true;
                        break;
                    }
                    Err(_) => break,
                }
                // Alpha scan is O(n); sample it rather than every frame.
                if frames.is_multiple_of(60) {
                    c.opaque.store(all_opaque(&buf), Relaxed);
                }
                if frames == 0 {
                    *lock(&c.status) = "ok".into();
                }
                frames += 1;
                spare = publish(&c.slot, buf);
            }
        }
        if let Some(mut ch) = lock(&c.child).take() {
            let _ = ch.kill();
            let _ = ch.wait(); // reap: no zombies
        }
        if let Some(d) = drain {
            let _ = d.join();
        }
        if c.stop.load(Relaxed) {
            break;
        }
        if c.noloop_media && clean_eof && frames > 0 {
            *lock(&c.status) = "ended".into();
            c.log.log(format!("[{}] media ended", c.name));
            while !c.stop.load(Relaxed) {
                thread::sleep(Duration::from_millis(200));
            }
            break;
        }
        if started.elapsed() > Duration::from_secs(10) {
            backoff = Duration::from_secs(1);
        }
        let le = lock(&last_err).clone();
        *lock(&c.status) = format!(
            "error: {}",
            if le.is_empty() { "capture ended" } else { &le }
        );
        c.log.log(format!(
            "[{}] capture stopped after {frames} frames; retry in {}s",
            c.name,
            backoff.as_secs()
        ));
        sleep_interruptible(backoff, &c.stop);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

// ----------------------------------------------------------------------------
// Compositor
// ----------------------------------------------------------------------------
#[derive(Clone)]
struct Layer {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    opacity: u8,
    slot: FrameSlot,
    opaque: Arc<AtomicBool>,
}

struct PreviewShared {
    buf: Mutex<(Vec<u8>, usize, usize)>, // RGB8, w, h
    seq: AtomicU64,
    req: AtomicU64, // (w << 32) | h requested by the UI
    /// v1.3.2: last `seq` the UI actually drew (compositor back-pressure).
    drawn: AtomicU64,
}

struct CompShared {
    layers: Mutex<Arc<Vec<Layer>>>,
    canvas: Mutex<Option<Arc<Vec<u8>>>>,
    stop: AtomicBool,
    cw: u32,
    ch: u32,
    fps: u32,
    compose_us: AtomicU64,
    fps_milli: AtomicU64,
    late: AtomicU64,
}

struct Compositor {
    sh: Arc<CompShared>,
    thread: Option<JoinHandle<()>>,
}

impl Compositor {
    fn start(cw: u32, ch: u32, fps: u32, pv: Arc<PreviewShared>) -> Self {
        let sh = Arc::new(CompShared {
            layers: Mutex::new(Arc::new(vec![])),
            canvas: Mutex::new(None),
            stop: AtomicBool::new(false),
            cw,
            ch,
            fps,
            compose_us: AtomicU64::new(0),
            fps_milli: AtomicU64::new(0),
            late: AtomicU64::new(0),
        });
        let s2 = sh.clone();
        let thread = thread::Builder::new()
            .name("compositor".into())
            .spawn(move || comp_loop(s2, pv))
            .ok();
        Self { sh, thread }
    }
    fn stop(mut self) {
        self.sh.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Exact round(x / 255) for x <= 255*255 without a hardware divide.
#[inline(always)]
fn div255(x: u32) -> u32 {
    let x = x + 128;
    (x + (x >> 8)) >> 8
}

/// Clip + blit one layer. Fast path: memcpy rows when fully opaque.
fn blit(dst: &mut [u8], cw: usize, ch: usize, src: &[u8], l: &Layer, opaque: bool) {
    let (sw, sh) = (l.w as usize, l.h as usize);
    if src.len() < sw * sh * 4 || l.opacity == 0 {
        return;
    }
    let (lx, ly) = (l.x as i64, l.y as i64);
    let x0 = lx.max(0);
    let y0 = ly.max(0);
    let x1 = (lx + sw as i64).min(cw as i64);
    let y1 = (ly + sh as i64).min(ch as i64);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let n = (x1 - x0) as usize;
    let sx0 = (x0 - lx) as usize;
    let op = l.opacity as u32;
    let fast = opaque && op == 255;
    for y in y0..y1 {
        let sy = (y - ly) as usize;
        let s = &src[(sy * sw + sx0) * 4..(sy * sw + sx0 + n) * 4];
        let di = (y as usize * cw + x0 as usize) * 4;
        let d = &mut dst[di..di + n * 4];
        if fast {
            d.copy_from_slice(s);
            continue;
        }
        // as_chunks yields [u8; 4] arrays: no per-pixel bounds checks in the blend.
        for (dp, sp) in d
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(s.as_chunks::<4>().0)
        {
            let a = div255(sp[3] as u32 * op);
            if a == 0 {
                continue;
            }
            if a == 255 {
                *dp = *sp;
                continue;
            }
            let ia = 255 - a;
            dp[0] = div255(sp[0] as u32 * a + dp[0] as u32 * ia) as u8;
            dp[1] = div255(sp[1] as u32 * a + dp[1] as u32 * ia) as u8;
            dp[2] = div255(sp[2] as u32 * a + dp[2] as u32 * ia) as u8;
            dp[3] = 255;
        }
    }
}

fn comp_loop(sh: Arc<CompShared>, pv: Arc<PreviewShared>) {
    let (cw, ch) = (sh.cw as usize, sh.ch as usize);
    let flen = cw * ch * 4;
    // v1.3.2: the black frame is a shared Arc, so an empty scene can publish
    // it as-is instead of copying 8 MB into a fresh canvas every tick.
    let black: Arc<Vec<u8>> = Arc::new([0u8, 0, 0, 255].repeat(cw * ch));
    let mut pool: Vec<Arc<Vec<u8>>> = Vec::new();
    let period_ns = 1_000_000_000u64 / sh.fps.max(1) as u64;
    let mut t0 = Instant::now();
    let mut n: u64 = 0;
    let (mut fps_t, mut fps_n) = (Instant::now(), 0u64);
    let mut xmap: Vec<usize> = Vec::new();
    let mut pbuf: Vec<u8> = Vec::new();
    // v1.3.2: (w, h, was_empty) of the last preview built.
    let mut last_pv: (usize, usize, bool) = (0, 0, false);
    while !sh.stop.load(Relaxed) {
        let ts = Instant::now();
        let layers = lock(&sh.layers).clone();
        // Hold each slot lock only for an Arc clone.
        let frames: Vec<Option<Arc<Vec<u8>>>> =
            layers.iter().map(|l| lock(&l.slot).clone()).collect();
        // v1.3.2: nothing to draw (no visible sources, or none has produced a
        // frame yet): skip compositing entirely and re-publish the black frame.
        let empty = frames.iter().all(Option::is_none);
        let frame: Arc<Vec<u8>> = if empty {
            black.clone()
        } else {
            // Recycle a canvas buffer nobody else references (encoders release theirs after write).
            let mut arc = match pool.iter().position(|a| Arc::strong_count(a) == 1) {
                Some(i) => pool.swap_remove(i),
                None => Arc::new(vec![0u8; flen]),
            };
            if Arc::get_mut(&mut arc).is_none() {
                arc = Arc::new(vec![0u8; flen]);
            }
            let buf = Arc::get_mut(&mut arc).expect("unique canvas buffer");
            // Skip the 8 MB clear when the bottom layer is opaque and covers the canvas.
            let covered = match (layers.first(), frames.first()) {
                (Some(l), Some(Some(_))) => {
                    l.opacity == 255
                        && l.opaque.load(Relaxed)
                        && l.x <= 0
                        && l.y <= 0
                        && l.x as i64 + l.w as i64 >= cw as i64
                        && l.y as i64 + l.h as i64 >= ch as i64
                }
                _ => false,
            };
            if !covered {
                buf.copy_from_slice(&black);
            }
            for (l, f) in layers.iter().zip(frames.iter()) {
                if let Some(f) = f {
                    blit(buf, cw, ch, f, l, l.opaque.load(Relaxed));
                }
            }
            if pool.len() < 6 {
                pool.push(arc.clone());
            }
            arc
        };
        // Nearest-neighbour downscale into RGB preview, outside the shared lock.
        let req = pv.req.load(Relaxed);
        let (pw, ph) = ((req >> 32) as usize, (req & 0xffff_ffff) as usize);
        // v1.3.2: build a preview only once the UI has drawn the previous one
        // (at 60 fps the UI shows ~30, so half the builds were thrown away,
        // and all of them while the window is minimised), and never rebuild
        // an unchanged black preview.
        let ui_ready = pv.drawn.load(Relaxed) >= pv.seq.load(Relaxed);
        let same_black = empty && last_pv == (pw, ph, true);
        if pw > 0 && ph > 0 && pw <= 8192 && ph <= 8192 && ui_ready && !same_black {
            if xmap.len() != pw {
                xmap = (0..pw).map(|x| (x * cw / pw).min(cw - 1) * 4).collect();
            }
            pbuf.resize(pw * ph * 3, 0);
            let src = &frame[..];
            let mut prev_sy = usize::MAX;
            for y in 0..ph {
                let sy = (y * ch / ph).min(ch - 1);
                let row_bytes = pw * 3;
                if sy == prev_sy {
                    // v1.3.2: preview taller than the canvas: repeat the row
                    // with one memcpy instead of re-gathering every pixel.
                    pbuf.copy_within((y - 1) * row_bytes..y * row_bytes, y * row_bytes);
                    continue;
                }
                prev_sy = sy;
                let row = &src[sy * cw * 4..(sy + 1) * cw * 4];
                let out = &mut pbuf[y * row_bytes..(y + 1) * row_bytes];
                for (o, &si) in out.as_chunks_mut::<3>().0.iter_mut().zip(xmap.iter()) {
                    o[0] = row[si + 2];
                    o[1] = row[si + 1];
                    o[2] = row[si];
                }
            }
            {
                let mut g = lock(&pv.buf);
                std::mem::swap(&mut g.0, &mut pbuf);
                g.1 = pw;
                g.2 = ph;
            }
            pv.seq.fetch_add(1, Relaxed);
            last_pv = (pw, ph, empty);
        }
        *lock(&sh.canvas) = Some(frame);
        sh.compose_us
            .store(ts.elapsed().as_micros() as u64, Relaxed);

        fps_n += 1;
        let el = fps_t.elapsed();
        if el >= Duration::from_secs(1) {
            sh.fps_milli
                .store((fps_n as f64 * 1000.0 / el.as_secs_f64()) as u64, Relaxed);
            fps_t = Instant::now();
            fps_n = 0;
        }
        // Drift-free pacing against an absolute schedule.
        n += 1;
        let target = t0 + Duration::from_nanos(period_ns.saturating_mul(n));
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        } else if now - target > Duration::from_nanos(period_ns * 3) {
            sh.late.fetch_add(1, Relaxed);
            t0 = now;
            n = 0;
        }
    }
}

// ----------------------------------------------------------------------------
// Streaming services & multistream destinations (v1.1.0)
// ----------------------------------------------------------------------------
const MAX_DESTINATIONS: usize = 8;

/// fifo pseudo-muxer options for every stream slave: each destination
/// reconnects on its own (OBS-like: 2 s delay, 25 tries) without restarting
/// the encoder or disturbing the other destinations.
const FIFO_OPTS: &str = "attempt_recovery=1:recover_any_error=1:recovery_wait_time=2:\
max_recovery_attempts=25:drop_pkts_on_overflow=1:restart_with_keyframe=1:queue_size=600";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
struct Destination {
    name: String,
    service: String,
    url: String,
    key: String,
    enabled: bool,
}

struct ServiceInfo {
    id: &'static str,
    label: &'static str,
    url: &'static str,
    /// Ingest limits used for pre-flight warnings (0 = not checked).
    max_kbps: u32,
    keyint_sec: u32,
    tls_only: bool,
    key_required: bool,
    hint: &'static str,
}

const SERVICES: &[ServiceInfo] = &[
    ServiceInfo {
        id: "kick",
        label: "Kick",
        url: "rtmps://fa723fc1b171.global-contribute.live-video.net/app",
        max_kbps: 8000,
        keyint_sec: 2,
        tls_only: true,
        key_required: true,
        hint: "Kick: Creator Dashboard → Channel → Stream URL and Key. Paste BOTH values \
(your server URL may differ from the default). RTMPS only; CBR, 2 s keyframes, \
1000–8000 kbps.",
    },
    ServiceInfo {
        id: "rumble",
        label: "Rumble",
        url: "rtmp://rtmp.rumble.com/live",
        max_kbps: 0,
        keyint_sec: 0,
        tls_only: false,
        key_required: true,
        hint: "Rumble: Go Live → create the stream → Get Streamer Configuration. Paste the \
Stream URL and Stream Key shown there. Keys are per-event unless you enable a static key.",
    },
    ServiceInfo {
        id: "twitch",
        label: "Twitch",
        url: "rtmp://live.twitch.tv/app",
        max_kbps: 0,
        keyint_sec: 2,
        tls_only: false,
        key_required: true,
        hint: "Twitch: Creator Dashboard → Settings → Stream → Primary Stream key.",
    },
    ServiceInfo {
        id: "youtube",
        label: "YouTube",
        url: "rtmp://a.rtmp.youtube.com/live2",
        max_kbps: 0,
        keyint_sec: 0,
        tls_only: false,
        key_required: true,
        hint: "YouTube: Studio → Go Live → Stream → Stream key.",
    },
    ServiceInfo {
        id: "custom",
        label: "Custom",
        url: "",
        max_kbps: 0,
        keyint_sec: 0,
        tls_only: false,
        key_required: false,
        hint: "Any RTMP / RTMPS / SRT ingest. The key is appended to the URL \
(ignored for srt:// — put streamid/passphrase in the URL instead).",
    },
];

fn service_info(id: &str) -> &'static ServiceInfo {
    SERVICES
        .iter()
        .find(|s| s.id == id)
        .unwrap_or(&SERVICES[SERVICES.len() - 1])
}

fn guess_service(url: &str) -> &'static str {
    let u = url.to_ascii_lowercase();
    if u.contains("fa723fc1b171.") || u.contains("kick.com") {
        "kick"
    } else if u.contains("rumble.com") {
        "rumble"
    } else if u.contains("twitch.tv") {
        "twitch"
    } else if u.contains("youtube.com") {
        "youtube"
    } else {
        "custom"
    }
}

/// URL/key characters that are safe both as an ffmpeg argument and inside the
/// tee muxer slave list (where | [ ] \ are syntax). Rejecting beats escaping.
fn url_chars_ok(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_graphic() && !"|[]\\'\"<>`{}^".contains(c))
}

/// Validate one destination and build its full ingest URL.
fn dest_target(d: &Destination) -> Result<String, String> {
    let svc = service_info(&d.service);
    let (url, key) = (d.url.trim(), d.key.trim());
    let lu = url.to_ascii_lowercase();
    let n = if d.name.is_empty() {
        svc.label
    } else {
        d.name.as_str()
    };
    if !["rtmp://", "rtmps://", "srt://"]
        .iter()
        .any(|p| lu.starts_with(p))
    {
        return Err(format!(
            "{n}: server URL must start with rtmp://, rtmps:// or srt://"
        ));
    }
    if svc.tls_only && !lu.starts_with("rtmps://") {
        return Err(format!("{n}: {} only accepts RTMPS (rtmps://…)", svc.label));
    }
    if !url_chars_ok(url) {
        return Err(format!(
            "{n}: server URL contains characters that are not allowed (spaces, quotes, | [ ] \\ …)"
        ));
    }
    if !key.is_empty() && !url_chars_ok(key) {
        return Err(format!(
            "{n}: stream key contains characters that are not allowed"
        ));
    }
    if svc.key_required && key.is_empty() {
        return Err(format!("{n}: stream key is required"));
    }
    Ok(if key.is_empty() || lu.starts_with("srt://") {
        url.to_string()
    } else {
        format!("{}/{}", url.trim_end_matches('/'), key)
    })
}

/// Pre-flight for "Start Streaming": hard errors abort, warnings are logged.
/// Returns ((name, url) per enabled destination, warnings).
type StreamPlan = (Vec<(String, String)>, Vec<String>);
fn stream_preflight(s: &Settings, ff: &FfInfo) -> Result<StreamPlan, String> {
    let enabled: Vec<&Destination> = s.destinations.iter().filter(|d| d.enabled).collect();
    if enabled.is_empty() {
        return Err(
            "No stream destination is enabled.\nAdd one in Controls → Stream Destinations (Ctrl+D)."
                .into(),
        );
    }
    let mut targets = vec![];
    let mut warns = vec![];
    for d in enabled {
        let url = dest_target(d)?;
        let svc = service_info(&d.service);
        if url.to_ascii_lowercase().starts_with("rtmps://")
            && !ff.out_protocols.is_empty()
            && !ff.out_protocols.contains("rtmps")
        {
            return Err(format!(
                "{}: this ffmpeg build has no RTMPS (TLS) support. Install an ffmpeg built with OpenSSL/GnuTLS.",
                d.name
            ));
        }
        if svc.max_kbps > 0 && s.video_kbps > svc.max_kbps {
            warns.push(format!(
                "{}: video bitrate {} kbps is above {}'s {} kbps ingest limit",
                d.name, s.video_kbps, svc.label, svc.max_kbps
            ));
        }
        if svc.keyint_sec > 0 && s.keyint_sec != svc.keyint_sec {
            warns.push(format!(
                "{}: {} expects a {} s keyframe interval (Settings → Keyint s is {})",
                d.name, svc.label, svc.keyint_sec, s.keyint_sec
            ));
        }
        targets.push((d.name.clone(), url));
    }
    if s.audio_api == "none"
        || (s.mic_device.trim().is_empty() && s.desktop_device.trim().is_empty())
    {
        warns.push("no audio device configured — sending a silent audio track".into());
    }
    Ok((targets, warns))
}

fn dest_summary(v: &[Destination]) -> String {
    let on: Vec<&str> = v
        .iter()
        .filter(|d| d.enabled)
        .map(|d| d.name.as_str())
        .collect();
    match (v.len(), on.len()) {
        (0, _) => "none — click Edit… to add Kick, Rumble, …".into(),
        (_, 0) => format!("{} configured, none enabled", v.len()),
        _ => format!("{} (of {})", on.join(", "), v.len()),
    }
}

// ----------------------------------------------------------------------------
// Output sessions (record / stream) — one ffmpeg encoder each
// ----------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Debug)]
enum OutKind {
    Record,
    Stream,
}

/// The encoder's audio input: always the one pre-mixed, pre-processed
/// stream from the AudioHub (`audio_in` = ffmpeg URL of its private pipe).
fn audio_inputs(ff: &FfInfo, audio_in: Option<&str>) -> Vec<Vec<String>> {
    let Some(url) = audio_in else {
        return vec![];
    };
    let q: &[&str] = if ff.input_queue_opt() {
        &["-thread_queue_size", "1024"]
    } else {
        &[]
    };
    vec![
        q.iter()
            .chain(&["-f", "f32le", "-ar", "48000", "-ac", "2", "-i"])
            .map(|x| x.to_string())
            .chain([url.to_string()])
            .collect(),
    ]
}

/// Pure function: settings -> encoder argv. Kept side-effect free for testing.
/// `targets`: one file path for Record; one or more ingest URLs for Stream.
/// `audio_in`: URL of the AudioHub pipe (None = no audio track / silence).
fn encoder_args(
    s: &Settings,
    ff: &FfInfo,
    kind: OutKind,
    targets: &[String],
    audio_in: Option<&str>,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut p = |v: &[&str]| a.extend(v.iter().map(|x| x.to_string()));
    p(&[
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-nostats",
    ]);
    if kind == OutKind::Record {
        p(&["-n"]); // never overwrite an existing file
    }
    let enc = s.encoder.as_str();
    if enc == "h264_vaapi" {
        p(&["-vaapi_device", &s.vaapi_device]);
    }
    let size = format!("{}x{}", s.canvas_w, s.canvas_h);
    let fps = s.fps.to_string();
    if ff.input_queue_opt() {
        p(&["-thread_queue_size", "64"]);
    }
    p(&[
        "-use_wallclock_as_timestamps",
        "1",
        "-f",
        "rawvideo",
        "-pix_fmt",
        PIX,
        "-video_size",
        &size,
        "-framerate",
        &fps,
        "-i",
        "pipe:0",
    ]);
    let auds = audio_inputs(ff, audio_in);
    for ai in &auds {
        a.extend(ai.iter().cloned());
    }
    // Most ingests (Kick/IVS, YouTube) reject or misbehave on video-only
    // streams: synthesise silence when no audio device is configured.
    let silent = kind == OutKind::Stream && auds.is_empty();
    let mut p = |v: &[&str]| a.extend(v.iter().map(|x| x.to_string()));
    if silent {
        // -re: lavfi sources run as fast as possible otherwise, flooding the
        // muxer with audio far ahead of the (wallclock-paced) video.
        p(&[
            "-re",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=channel_layout=stereo:sample_rate=48000",
        ]);
    }
    let vf = match enc {
        "h264_vaapi" => "format=nv12,hwupload",
        "h264_qsv" => "format=nv12",
        _ => "format=yuv420p",
    };
    let fc = format!("[0:v]{vf}[vout]");
    p(&["-filter_complex", &fc, "-map", "[vout]"]);
    if !auds.is_empty() || silent {
        p(&["-map", "1:a"]);
    }
    p(&["-c:v", enc]);
    match enc {
        "libx264" => {
            p(&["-preset", &s.x264_preset, "-profile:v", "high"]);
            if kind == OutKind::Stream {
                p(&["-x264-params", "nal-hrd=cbr"]); // true CBR: Kick/Twitch requirement
            }
        }
        "h264_nvenc" => p(&["-preset", "p4", "-rc", "cbr"]),
        "h264_qsv" => p(&["-preset", "veryfast"]),
        "h264_amf" => p(&["-quality", "speed"]),
        "h264_videotoolbox" => p(&["-realtime", "1"]),
        _ => {}
    }
    let kb = format!("{}k", s.video_kbps);
    let buf = format!("{}k", s.video_kbps.saturating_mul(2));
    let g = (s.fps * s.keyint_sec).to_string();
    p(&["-b:v", &kb, "-maxrate", &kb, "-bufsize", &buf, "-g", &g]);
    if ff.has_fps_mode() {
        p(&["-fps_mode", "cfr"]);
    } else {
        p(&["-vsync", "cfr"]);
    }
    p(&["-r", &fps]);
    if !auds.is_empty() || silent {
        let ab = format!("{}k", s.audio_kbps);
        p(&["-c:a", "aac", "-b:a", &ab, "-ar", "48000", "-ac", "2"]);
    }
    match kind {
        OutKind::Record => {
            match s.rec_format.as_str() {
                "mp4" => p(&["-f", "mp4", "-movflags", "+faststart"]),
                "mov" => p(&["-f", "mov"]),
                "flv" => p(&["-f", "flv"]),
                "ts" => p(&["-f", "mpegts"]),
                _ => p(&["-f", "matroska"]),
            }
            let t = targets.first().map(String::as_str).unwrap_or("");
            // v1.3.1 FIX (security): always a local file. file_arg() lets
            // "scheme://" through, so a profile with rec_dir = "http://host/"
            // made ffmpeg upload every recording to that host.
            p(&[&local_file_arg(t)]);
        }
        OutKind::Stream => {
            // ONE encode fanned out by tee; each slave sits behind a fifo so a
            // slow/dead destination neither blocks nor kills the others and
            // reconnects on its own. FLV/TS slaves need codec extradata up
            // front when muxed through tee, hence global_header.
            let slaves = targets
                .iter()
                .map(|t| {
                    if t.to_ascii_lowercase().starts_with("srt://") {
                        format!("[f=mpegts:onfail=ignore]{t}")
                    } else {
                        format!("[f=flv:flvflags=no_duration_filesize:onfail=ignore]{t}")
                    }
                })
                .collect::<Vec<_>>()
                .join("|");
            p(&[
                "-flags:v",
                "+global_header",
                "-flags:a",
                "+global_header",
                "-f",
                "tee",
                "-use_fifo",
                "1",
                "-fifo_options",
                FIFO_OPTS,
                &slaves,
            ]);
        }
    }
    a
}

fn secrets_of(s: &Settings) -> Vec<String> {
    let mut v = vec![];
    let pairs = s
        .destinations
        .iter()
        .map(|d| (d.url.as_str(), d.key.as_str()))
        .chain(std::iter::once((
            s.stream_url.as_str(),
            s.stream_key.as_str(),
        )));
    for (url, key) in pairs {
        v.push(key.trim().to_string());
        // rtmp://user:pass@host — redact userinfo; SRT passphrase/streamid in query.
        let u = url.trim();
        if let Some(rest) = u.split_once("://").map(|x| x.1)
            && let Some((ui, _)) = rest.split_once('@')
        {
            v.push(ui.to_string());
        }
        if let Some((_, q)) = u.split_once('?') {
            for kv in q.split('&') {
                if let Some((k, val)) = kv.split_once('=')
                    && (k.eq_ignore_ascii_case("passphrase") || k.eq_ignore_ascii_case("streamid"))
                {
                    v.push(val.to_string());
                }
            }
        }
    }
    v.retain(|x| x.len() >= 4);
    v.sort();
    v.dedup();
    v
}

// ----------------------------------------------------------------------------
// Native audio engine (v1.3.0): capture → per-source DSP → mixer → encoders
// ----------------------------------------------------------------------------
// Every backend now feeds LiteCast's own mixer:
//   * Windows `wasapi`: in-process WASAPI mic + speaker loopback;
//   * pulse / alsa / dshow / avfoundation: one small ffmpeg capture child per
//     device, decoding to 48 kHz stereo f32 on its stdout.
// Each capture thread fills a SrcRing. One mixer thread, paced to the wall
// clock, runs every source through its DSP chain in 10 ms blocks:
//   mic:     high-pass 80 Hz → noise gate → compressor → auto-level → trim
//   desktop: auto-level → trim → ducking (under voice)
//   master:  sum → peak limiter (−1 dBFS)
// then fans the block out to every running output (one private pipe each)
// and publishes VU meters for the UI. Auto-level drives both sources to the
// same loudness target, which is what keeps mic and system audio balanced.
//
// WASAPI loopback delivers *no packets* while nothing plays; the mixer
// zero-fills, so ffmpeg always gets a continuous, correctly-timed stream.

const AUD_RATE: usize = 48_000;
const AUD_CH: usize = 2;
/// Frames per 10 ms block.
const AUD_FRAMES: usize = AUD_RATE / 100;
/// 10 ms of interleaved stereo samples.
const AUD_CHUNK: usize = AUD_FRAMES * AUD_CH;
/// A ring starts (or restarts after an underrun) only once 30 ms are
/// buffered, so capture jitter does not turn into crackle.
const RING_PRIME: usize = AUD_CHUNK * 3;
/// Above 250 ms the ring is trimmed back to the prime level: this bounds
/// latency and absorbs device-clock vs wall-clock drift.
const RING_MAX: usize = AUD_RATE / 4 * AUD_CH;
const DB_FLOOR: f32 = -90.0;

fn db(x: f32) -> f32 {
    if x <= 1e-9 {
        DB_FLOOR
    } else {
        (20.0 * x.log10()).max(DB_FLOOR)
    }
}
fn undb(d: f32) -> f32 {
    10f32.powf(d / 20.0)
}
/// One-pole smoothing coefficient for a time constant, per sample.
fn coef(ms: f32) -> f32 {
    1.0 - (-1.0 / (ms * 0.001 * AUD_RATE as f32)).exp()
}

/// User-facing processing settings (persisted in the profile, live-tunable).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
struct AudioDsp {
    /// Loudness both sources are levelled to (short-term RMS, dBFS).
    target_db: i32,
    mic_gate: bool,
    gate_db: i32,
    mic_comp: bool,
    comp_db: i32,
    comp_ratio: u32,
    mic_level: bool,
    desk_level: bool,
    /// Lower desktop audio by this many dB while the mic is active (0 = off).
    duck_db: u32,
    mic_trim_db: i32,
    desk_trim_db: i32,
    mic_mute: bool,
    desk_mute: bool,
    limiter: bool,
}

impl Default for AudioDsp {
    fn default() -> Self {
        Self {
            target_db: -20,
            mic_gate: true,
            gate_db: -50,
            mic_comp: true,
            comp_db: -26,
            comp_ratio: 3,
            mic_level: true,
            desk_level: true,
            duck_db: 0,
            mic_trim_db: 0,
            desk_trim_db: 0,
            mic_mute: false,
            desk_mute: false,
            limiter: true,
        }
    }
}

impl AudioDsp {
    /// Clamp everything loaded from a (possibly hostile) profile.
    fn sanitize(&mut self) {
        self.target_db = self.target_db.clamp(-36, -6);
        self.gate_db = self.gate_db.clamp(-80, -20);
        self.comp_db = self.comp_db.clamp(-50, -6);
        self.comp_ratio = self.comp_ratio.clamp(1, 20);
        self.duck_db = self.duck_db.min(30);
        self.mic_trim_db = self.mic_trim_db.clamp(-30, 20);
        self.desk_trim_db = self.desk_trim_db.clamp(-30, 20);
    }
}

/// RBJ biquad, transposed direct form II, one per channel.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn highpass(f: f32) -> Self {
        let w = 2.0 * std::f32::consts::PI * f / AUD_RATE as f32;
        let (c, alpha) = (w.cos(), w.sin() / (2.0 * std::f32::consts::FRAC_1_SQRT_2));
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + c) / 2.0 / a0,
            b1: -(1.0 + c) / a0,
            b2: (1.0 + c) / 2.0 / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - alpha) / a0,
            ..Default::default()
        }
    }
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        // v1.3.1 FIX: flush denormals. During silence the filter state decays
        // into subnormal floats, which are up to ~100x slower on x86.
        if self.z1.abs() < 1e-25 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-25 {
            self.z2 = 0.0;
        }
        y
    }
}

fn block_rms_db(b: &[f32]) -> f32 {
    if b.is_empty() {
        return DB_FLOOR;
    }
    let ms = b.iter().map(|x| x * x).sum::<f32>() / b.len() as f32;
    db(ms.sqrt())
}
fn block_peak(b: &[f32]) -> f32 {
    b.iter().fold(0f32, |m, x| m.max(x.abs()))
}

/// Per-source processing state.
struct SrcDsp {
    is_mic: bool,
    hp: [Biquad; 2],
    gate_open: bool,
    gate_hold: u32,
    gate_gain: f32,
    comp_gr_db: f32,
    /// Smoothed mean-square energy of active blocks (auto-level detector).
    lvl_ms: f32,
    lvl_init: bool,
    agc_db: f32,
    gain: f32,
    duck_db: f32,
    /// Meter outputs.
    m_rms: f32,
    m_peak: f32,
    m_hold: f32,
    m_hold_n: u32,
    active: bool,
}

impl SrcDsp {
    fn new(is_mic: bool) -> Self {
        Self {
            is_mic,
            hp: [Biquad::highpass(80.0); 2],
            gate_open: false,
            gate_hold: 0,
            gate_gain: 0.0,
            comp_gr_db: 0.0,
            lvl_ms: 0.0,
            lvl_init: false,
            agc_db: 0.0,
            gain: 1.0,
            duck_db: 0.0,
            m_rms: DB_FLOOR,
            m_peak: DB_FLOOR,
            m_hold: DB_FLOOR,
            m_hold_n: 0,
            active: false,
        }
    }

    /// Process one interleaved stereo block in place. `duck` = dB of ducking
    /// requested for this block (desktop only).
    fn process(&mut self, b: &mut [f32], p: &AudioDsp, duck: f32) {
        let mic = self.is_mic;
        // 1. High-pass (mic): rumble/handling noise would otherwise drive the
        //    compressor and auto-level.
        if mic {
            for f in b.as_chunks_mut::<AUD_CH>().0 {
                f[0] = self.hp[0].run(f[0]);
                f[1] = self.hp[1].run(f[1]);
            }
        }
        let in_db = block_rms_db(b);
        // 2. Noise gate with hysteresis + 200 ms hold; closes to −40 dB (not
        //    hard mute) so room tone does not chop.
        if mic && p.mic_gate {
            let thr = p.gate_db as f32;
            if in_db > thr {
                self.gate_open = true;
                self.gate_hold = 20;
            } else if in_db < thr - 6.0 {
                if self.gate_hold > 0 {
                    self.gate_hold -= 1;
                } else {
                    self.gate_open = false;
                }
            }
            let target = if self.gate_open { 1.0 } else { undb(-40.0) };
            let c = if target > self.gate_gain {
                coef(2.0)
            } else {
                coef(120.0)
            };
            for f in b.as_chunks_mut::<AUD_CH>().0 {
                self.gate_gain += (target - self.gate_gain) * c;
                f[0] *= self.gate_gain;
                f[1] *= self.gate_gain;
            }
        } else {
            self.gate_open = in_db > p.gate_db as f32;
            self.gate_gain = 1.0;
        }
        // 3. Compressor (mic): stereo-linked peak detector, 6 dB soft knee,
        //    5 ms attack / 150 ms release on the gain reduction.
        if mic && p.mic_comp {
            let (thr, r, knee) = (p.comp_db as f32, p.comp_ratio.max(1) as f32, 6.0f32);
            let slope = 1.0 - 1.0 / r;
            let (ca, cr) = (coef(5.0), coef(150.0));
            for f in b.as_chunks_mut::<AUD_CH>().0 {
                let x = db(f[0].abs().max(f[1].abs()));
                let over = x - thr;
                let gr = if over <= -knee / 2.0 {
                    0.0
                } else if over >= knee / 2.0 {
                    over * slope
                } else {
                    slope * (over + knee / 2.0).powi(2) / (2.0 * knee)
                };
                let c = if gr > self.comp_gr_db { ca } else { cr };
                self.comp_gr_db += (gr - self.comp_gr_db) * c;
                let g = undb(-self.comp_gr_db);
                f[0] *= g;
                f[1] *= g;
            }
        } else {
            self.comp_gr_db = 0.0;
        }
        // 4. Auto-level: 400 ms loudness detector on *active* blocks only (so
        //    silence/noise is never boosted), slew-limited gain toward target.
        let lvl_on = if mic { p.mic_level } else { p.desk_level };
        let pre = block_rms_db(b);
        let act = if mic {
            if p.mic_gate {
                self.gate_open
            } else {
                pre > -50.0
            }
        } else {
            pre > -55.0
        };
        self.active = act;
        if lvl_on {
            if act {
                let ms = undb(pre).powi(2);
                if self.lvl_init {
                    let a = 1.0 - (-0.01f32 / 0.4).exp();
                    self.lvl_ms += (ms - self.lvl_ms) * a;
                } else {
                    self.lvl_ms = ms;
                    self.lvl_init = true;
                }
                let (lo, hi) = if mic { (-20.0, 24.0) } else { (-20.0, 15.0) };
                let want = (p.target_db as f32 - db(self.lvl_ms.sqrt())).clamp(lo, hi);
                // Down 10 dB/s, up 3 dB/s; and never let a sudden loud block
                // through more than 6 dB above target (anti-blast).
                let step = if want < self.agc_db { 0.10 } else { 0.03 };
                self.agc_db += (want - self.agc_db).clamp(-step, step);
                let blast = pre + self.agc_db - (p.target_db as f32 + 6.0);
                if blast > 0.0 {
                    self.agc_db -= blast;
                }
            }
        } else {
            self.agc_db = 0.0;
            self.lvl_init = false;
        }
        // 5. Trim, mute, ducking → one gain ramped across the block (no zipper).
        let (trim, mute) = if mic {
            (p.mic_trim_db, p.mic_mute)
        } else {
            (p.desk_trim_db, p.desk_mute)
        };
        let dc = if duck > self.duck_db { 0.25 } else { 0.02 }; // ~40 ms in, ~0.5 s out
        self.duck_db += (duck - self.duck_db) * dc;
        let g1 = if mute {
            0.0
        } else {
            undb(self.agc_db + trim as f32 - self.duck_db)
        };
        let (g0, n) = (self.gain, (b.len() / AUD_CH) as f32);
        for (i, f) in b.as_chunks_mut::<AUD_CH>().0.iter_mut().enumerate() {
            let g = g0 + (g1 - g0) * (i as f32 + 1.0) / n;
            f[0] *= g;
            f[1] *= g;
        }
        self.gain = g1;
        meter_update(
            &mut self.m_rms,
            &mut self.m_peak,
            &mut self.m_hold,
            &mut self.m_hold_n,
            b,
        );
    }
}

/// VU ballistics per 10 ms block: instant rise, 20 dB/s fall; peak hold 1.5 s.
fn meter_update(rms: &mut f32, peak: &mut f32, hold: &mut f32, hold_n: &mut u32, b: &[f32]) {
    let r = block_rms_db(b);
    let pk = db(block_peak(b));
    *rms = r.max(*rms - 0.2);
    *peak = pk.max(*peak - 0.2);
    if pk >= *hold {
        *hold = pk;
        *hold_n = 150;
    } else if *hold_n > 0 {
        *hold_n -= 1;
    } else {
        *hold = (*hold - 0.3).max(DB_FLOOR);
    }
}

/// Master peak limiter: instant attack, 100 ms release, −1 dBFS ceiling.
struct Limiter {
    g: f32,
    gr_db: f32,
}

impl Limiter {
    fn run(&mut self, b: &mut [f32], on: bool) {
        let ceil = undb(-1.0);
        let rel = coef(100.0);
        let mut min_g = 1.0f32;
        for f in b.as_chunks_mut::<AUD_CH>().0 {
            if on {
                let pk = f[0].abs().max(f[1].abs());
                if pk * self.g > ceil {
                    self.g = ceil / pk;
                } else {
                    self.g += (1.0 - self.g) * rel;
                }
                min_g = min_g.min(self.g);
                f[0] *= self.g;
                f[1] *= self.g;
            }
            f[0] = f[0].clamp(-1.0, 1.0);
            f[1] = f[1].clamp(-1.0, 1.0);
        }
        self.gr_db = -db(min_g);
    }
}

/// Snapshot published for the UI (~every 10 ms).
#[derive(Clone, Debug, Default)]
struct ChanMeter {
    label: String,
    rms: f32,
    peak: f32,
    hold: f32,
    /// Compressor / limiter gain reduction, dB (≥ 0).
    gr: f32,
    /// Auto-level gain, dB.
    agc: f32,
    active: bool,
    muted: bool,
}

#[derive(Clone, Debug, Default)]
struct Meters {
    chans: Vec<ChanMeter>,
    master: ChanMeter,
    target_db: f32,
    updated: Option<Instant>,
}

#[derive(Default)]
struct SrcRing {
    q: VecDeque<f32>,
    primed: bool,
}

impl SrcRing {
    fn push(&mut self, s: &[f32]) {
        // v1.3.1 FIX: a NaN/inf from a device (or a desynced byte stream)
        // used to enter the biquad/limiter state and silence or corrupt the
        // mix *permanently*; non-finite samples become 0 and wild values are
        // bounded (+12 dBFS) before any DSP sees them.
        self.q.extend(s.iter().map(|&x| {
            if x.is_finite() {
                x.clamp(-4.0, 4.0)
            } else {
                0.0
            }
        }));
        if self.q.len() > RING_MAX {
            let excess = self.q.len() - RING_PRIME;
            self.q.drain(..excess - excess % AUD_CH); // keep L/R alignment
        }
    }
    /// Fill `out` from the ring; silence while unprimed or on underrun.
    fn take(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        if !self.primed {
            if self.q.len() < RING_PRIME {
                return;
            }
            self.primed = true;
        }
        // v1.3.1 FIX: on underrun take whole frames only. Draining an odd
        // sample count left a lone R sample at the front of the ring, which
        // swapped left/right for the rest of the session.
        let n = out.len().min(self.q.len() / AUD_CH * AUD_CH);
        for (o, s) in out.iter_mut().zip(self.q.drain(..n)) {
            *o = s;
        }
        if n < out.len() {
            self.primed = false; // underrun: wait for a fresh cushion
        }
    }
}

/// Little-endian f32 PCM, as ffmpeg's `-f f32le` expects.
fn pcm_bytes(s: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(s.len() * 4);
    for x in s {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

type AudioChunk = Arc<Vec<u8>>;

/// v1.3.2: one output's audio feed, with a count of blocks it lost because
/// its encoder fell more than ~1 s behind (each lost block = 10 ms of drift).
struct Sub {
    tag: String,
    tx: std::sync::mpsc::SyncSender<AudioChunk>,
    drops: Arc<AtomicU64>,
    logged: u64,
    last_log: Option<Instant>,
}
type Subs = Arc<Mutex<Vec<Sub>>>;

/// v1.3.2: recycles the 10 ms PCM chunks instead of allocating ~100 per
/// second. Chunks go out in FIFO order, so the oldest one is the first to be
/// released by every encoder; it is reused once only the pool holds it.
#[derive(Default)]
struct ChunkPool {
    q: VecDeque<AudioChunk>,
    allocs: u64,
}

impl ChunkPool {
    /// 4 s of chunks: more than any subscriber can hold (1 s channel each).
    const CAP: usize = 400;
    fn get(&mut self, s: &[f32]) -> AudioChunk {
        let mut a = match self.q.front() {
            Some(f) if Arc::strong_count(f) == 1 => self.q.pop_front().expect("front exists"),
            _ => {
                self.allocs += 1;
                Arc::new(Vec::with_capacity(s.len() * 4))
            }
        };
        if let Some(v) = Arc::get_mut(&mut a) {
            v.clear();
            for x in s {
                v.extend_from_slice(&x.to_le_bytes());
            }
        } else {
            // Not reachable (count was 1), but never hand out stale audio.
            self.allocs += 1;
            a = Arc::new(pcm_bytes(s));
        }
        self.q.push_back(a.clone());
        if self.q.len() > Self::CAP {
            self.q.pop_front();
        }
        a
    }
}

/// One configured capture source.
#[derive(Clone, Debug, PartialEq)]
enum CapSpec {
    /// Native WASAPI device ("default" or friendly name); loopback = speakers.
    Wasapi { dev: String, loopback: bool },
    /// ffmpeg capture child; argv decodes the device to f32le on stdout.
    Ffmpeg { prog: String, args: Vec<String> },
}

/// The whole mixing pipeline. AppState keeps one running for live meters;
/// each output holds an `Arc` too, so a recording survives a settings change.
struct AudioHub {
    /// Capture-config fingerprint: a hub is reused only if unchanged.
    key: String,
    subs: Subs,
    stop: Arc<AtomicBool>,
    children: Vec<Arc<Mutex<Option<Child>>>>,
    threads: Vec<JoinHandle<()>>,
}

impl AudioHub {
    /// `sources`: (label, is_mic, spec).
    fn start(
        key: String,
        sources: Vec<(String, bool, CapSpec)>,
        dsp: Arc<Mutex<AudioDsp>>,
        meters: Arc<Mutex<Meters>>,
        log: &Logger,
    ) -> Arc<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let subs: Subs = Arc::default();
        let (mut threads, mut rings, mut children, mut chans) = (vec![], vec![], vec![], vec![]);
        for (label, is_mic, spec) in sources {
            let ring: Arc<Mutex<SrcRing>> = Arc::default();
            let child: Arc<Mutex<Option<Child>>> = Arc::default();
            rings.push(ring.clone());
            children.push(child.clone());
            chans.push((label.clone(), is_mic));
            let (stop, log) = (stop.clone(), log.clone());
            if let Ok(h) = thread::Builder::new()
                .name(format!("audio-{label}"))
                .spawn(move || capture_supervisor(label, spec, ring, child, stop, log))
            {
                threads.push(h);
            }
        }
        let (stop2, subs2, log2) = (stop.clone(), subs.clone(), log.clone());
        if let Ok(h) = thread::Builder::new()
            .name("audio-mixer".into())
            .spawn(move || mixer_loop(rings, chans, dsp, meters, subs2, stop2, log2))
        {
            threads.push(h);
        }
        Arc::new(Self {
            key,
            subs,
            stop,
            children,
            threads,
        })
    }

    /// ~1 s of buffering per subscriber; a stalled encoder loses chunks
    /// instead of stalling the mixer (or the other outputs).
    /// v1.3.2: also returns the subscriber's dropped-block counter.
    fn subscribe(&self, tag: &str) -> (std::sync::mpsc::Receiver<AudioChunk>, Arc<AtomicU64>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(100);
        let drops = Arc::new(AtomicU64::new(0));
        lock(&self.subs).push(Sub {
            tag: tag.to_string(),
            tx,
            drops: drops.clone(),
            logged: 0,
            last_log: None,
        });
        (rx, drops)
    }
}

impl Drop for AudioHub {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        for c in &self.children {
            if let Some(ch) = lock(c).as_mut() {
                let _ = ch.kill(); // unblocks the stdout reader
            }
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Wall-clock-paced producer: always emits exactly AUD_RATE frames per
/// second of real time (drift-free; catches up after scheduling hiccups).
fn mixer_loop(
    rings: Vec<Arc<Mutex<SrcRing>>>,
    chans: Vec<(String, bool)>,
    dsp: Arc<Mutex<AudioDsp>>,
    meters: Arc<Mutex<Meters>>,
    subs: Subs,
    stop: Arc<AtomicBool>,
    log: Logger,
) {
    let t0 = Instant::now();
    let mut pool = ChunkPool::default();
    let mut blocks: u64 = 0;
    let mut states: Vec<SrcDsp> = chans.iter().map(|c| SrcDsp::new(c.1)).collect();
    let mut tmp = vec![0f32; AUD_CHUNK];
    let mut mix = vec![0f32; AUD_CHUNK];
    let mut lim = Limiter { g: 1.0, gr_db: 0.0 };
    let (mut m_rms, mut m_peak, mut m_hold, mut m_hold_n) = (DB_FLOOR, DB_FLOOR, DB_FLOOR, 0u32);
    while !stop.load(Relaxed) {
        let due = t0.elapsed().as_millis() as u64 / 10;
        if blocks >= due {
            // v1.3.2: sleep until the next block is due (one wakeup per 10 ms
            // block) instead of polling every 3 ms (~300 wakeups/s).
            let next = t0 + Duration::from_millis((blocks + 1) * 10);
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            }
            continue;
        }
        // > 1 s behind (machine suspended?): skip ahead rather than burst.
        if due - blocks > 100 {
            blocks = due - 1;
        }
        let p = *lock(&dsp);
        while blocks < due {
            mix.fill(0.0);
            // Mic first: its activity drives desktop ducking in the same block.
            let mut voice = false;
            for pass in [true, false] {
                for ((ring, st), _) in rings.iter().zip(states.iter_mut()).zip(&chans) {
                    if st.is_mic != pass {
                        continue;
                    }
                    lock(ring).take(&mut tmp);
                    let duck = if !st.is_mic && voice && !p.mic_mute {
                        p.duck_db as f32
                    } else {
                        0.0
                    };
                    st.process(&mut tmp, &p, duck);
                    if st.is_mic && st.active {
                        voice = true;
                    }
                    for (m, x) in mix.iter_mut().zip(&tmp) {
                        *m += x;
                    }
                }
            }
            lim.run(&mut mix, p.limiter);
            meter_update(&mut m_rms, &mut m_peak, &mut m_hold, &mut m_hold_n, &mix);
            let c: AudioChunk = pool.get(&mix); // v1.3.2: recycled buffer
            lock(&subs).retain_mut(|sub| match sub.tx.try_send(c.clone()) {
                Ok(()) => true,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    // v1.3.2: was silent. A full queue means this encoder is
                    // > 1 s behind; the lost 10 ms shifts its audio earlier.
                    let n = sub.drops.fetch_add(1, Relaxed) + 1;
                    if sub
                        .last_log
                        .is_none_or(|t| t.elapsed() >= Duration::from_secs(10))
                    {
                        log.log(format!(
                            "[audio] {}: encoder is not keeping up, {} ms of audio dropped \
                             (+{} ms) — audio may drift; try a faster preset or lower resolution",
                            sub.tag,
                            n * 10,
                            (n - sub.logged) * 10
                        ));
                        sub.logged = n;
                        sub.last_log = Some(Instant::now());
                    }
                    true
                }
            });
            blocks += 1;
        }
        let mut m = lock(&meters);
        m.chans = states
            .iter()
            .zip(&chans)
            .map(|(s, c)| ChanMeter {
                label: c.0.clone(),
                rms: s.m_rms,
                peak: s.m_peak,
                hold: s.m_hold,
                gr: s.comp_gr_db,
                agc: s.agc_db,
                active: s.active,
                muted: if s.is_mic { p.mic_mute } else { p.desk_mute },
            })
            .collect();
        m.master = ChanMeter {
            label: "Master".into(),
            rms: m_rms,
            peak: m_peak,
            hold: m_hold,
            gr: lim.gr_db,
            ..Default::default()
        };
        m.target_db = p.target_db as f32;
        m.updated = Some(Instant::now());
    }
}

/// Keeps one source alive: reopens after errors (device unplugged, default
/// device changed, capture child died) with backoff, until stopped.
fn capture_supervisor(
    label: String,
    spec: CapSpec,
    ring: Arc<Mutex<SrcRing>>,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
    log: Logger,
) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Relaxed) {
        let t = Instant::now();
        let r = match &spec {
            CapSpec::Wasapi { dev, loopback } => {
                capture_wasapi(&label, dev, *loopback, &ring, &stop, &log)
            }
            CapSpec::Ffmpeg { prog, args } => {
                capture_ffmpeg(&label, prog, args, &ring, &child, &stop, &log)
            }
        };
        if stop.load(Relaxed) {
            break;
        }
        if t.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_secs(1); // it had been working
        }
        let why = r.err().unwrap_or_else(|| "capture ended".into());
        log.log(format!(
            "[audio] {label}: {why} — retrying in {}s",
            backoff.as_secs()
        ));
        sleep_interruptible(backoff, &stop);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// ffmpeg capture child → stdout f32le → ring.
fn capture_ffmpeg(
    label: &str,
    prog: &str,
    args: &[String],
    ring: &Arc<Mutex<SrcRing>>,
    child: &Arc<Mutex<Option<Child>>>,
    stop: &AtomicBool,
    log: &Logger,
) -> Result<(), String> {
    let mut cmd = ffmpeg_cmd(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut ch = cmd
        .spawn()
        .map_err(|e| format!("cannot start ffmpeg: {e}"))?;
    let mut out = ch.stdout.take().ok_or("no stdout")?;
    let last = Arc::new(Mutex::new(String::new()));
    let drain = ch
        .stderr
        .take()
        .and_then(|e| spawn_stderr_drain(e, format!("audio {label}"), log.clone(), last.clone()));
    *lock(child) = Some(ch);
    if stop.load(Relaxed) {
        // Raced with hub shutdown: make sure the child cannot outlive it.
        if let Some(c) = lock(child).as_mut() {
            let _ = c.kill();
        }
    }
    log.log(format!("[audio] {label}: capturing via ffmpeg"));
    let mut buf = vec![0u8; 16 * 1024];
    let mut carry: Vec<u8> = Vec::with_capacity(16 * 1024 + 8);
    let mut samples: Vec<f32> = Vec::with_capacity(4096);
    loop {
        let n = match out.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            // v1.3.1 FIX: a signal-interrupted read is not EOF.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        carry.extend_from_slice(&buf[..n]);
        // v1.3.1 FIX: push whole stereo frames (8 bytes), not whole samples,
        // so every push keeps the ring L/R-aligned.
        const FRAME_BYTES: usize = 4 * AUD_CH;
        let whole = carry.len() / FRAME_BYTES * FRAME_BYTES;
        samples.clear();
        samples.extend(
            carry[..whole]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c)),
        );
        carry.drain(..whole);
        lock(ring).push(&samples);
    }
    if let Some(mut c) = lock(child).take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    if let Some(d) = drain {
        let _ = d.join();
    }
    let e = lock(&last).clone();
    Err(if e.is_empty() {
        "capture process exited".into()
    } else {
        e
    })
}

#[cfg(not(windows))]
fn capture_wasapi(
    _label: &str,
    _dev: &str,
    _loopback: bool,
    _ring: &Arc<Mutex<SrcRing>>,
    _stop: &AtomicBool,
    _log: &Logger,
) -> Result<(), String> {
    Err("WASAPI is only available on Windows".into())
}

#[cfg(windows)]
fn capture_wasapi(
    label: &str,
    dev: &str,
    loopback: bool,
    ring: &Arc<Mutex<SrcRing>>,
    stop: &AtomicBool,
    log: &Logger,
) -> Result<(), String> {
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
    let e = |x: wasapi::WasapiError| x.to_string();
    let _ = wasapi::initialize_mta(); // per thread; S_FALSE if already done
    let en = DeviceEnumerator::new().map_err(e)?;
    // Loopback = open the *render* endpoint, initialise it for capture.
    let dir = if loopback {
        Direction::Render
    } else {
        Direction::Capture
    };
    let device = if dev.eq_ignore_ascii_case("default") {
        en.get_default_device(dir).map_err(e)?
    } else {
        let coll = en.get_device_collection(dir).map_err(e)?;
        let mut found = None;
        for d in &coll {
            let d = d.map_err(e)?;
            if d.get_friendlyname().map(|n| n == dev).unwrap_or(false) {
                found = Some(d);
                break;
            }
        }
        found.ok_or_else(|| format!("device \"{dev}\" not found"))?
    };
    let name = device
        .get_friendlyname()
        .unwrap_or_else(|_| dev.to_string());
    // v1.3.1 FIX: WASAPI does not invalidate a stream when the user switches
    // the Windows default device, so "default" kept capturing the *old*
    // speakers/mic. Remember the endpoint id and re-check it every 2 s.
    let follow_default = dev.eq_ignore_ascii_case("default");
    let dev_id = device.get_id().unwrap_or_default();
    let mut next_check = Instant::now() + Duration::from_secs(2);
    let mut client = device.get_iaudioclient().map_err(e)?;
    // Shared mode + autoconvert: Windows resamples to 48 kHz stereo f32.
    let fmt = WaveFormat::new(32, 32, SampleType::Float, AUD_RATE, AUD_CH, None);
    let mode = StreamMode::PollingShared {
        autoconvert: true,
        buffer_duration_hns: 1_000_000, // 100 ms device buffer
    };
    client
        .initialize_client(&fmt, Direction::Capture, mode, None)
        .map_err(e)?;
    let cap = client.get_audiocaptureclient().map_err(e)?;
    client.start_stream().map_err(e)?;
    log.log(format!(
        "[audio] {label}: capturing \"{name}\"{}",
        if loopback { " (loopback)" } else { "" }
    ));
    let mut bytes: VecDeque<u8> = VecDeque::with_capacity(64 * 1024);
    let mut samples: Vec<f32> = Vec::with_capacity(16 * 1024);
    let res = loop {
        if stop.load(Relaxed) {
            break Ok(());
        }
        thread::sleep(Duration::from_millis(5));
        let mut err = None;
        loop {
            match cap.get_next_packet_size() {
                Ok(Some(n)) if n > 0 => {
                    if let Err(x) = cap.read_from_device_to_deque(&mut bytes) {
                        err = Some(e(x));
                        break;
                    }
                }
                Ok(_) => break,
                Err(x) => {
                    err = Some(e(x));
                    break;
                }
            }
        }
        if let Some(x) = err {
            break Err(x);
        }
        if follow_default && Instant::now() >= next_check {
            next_check = Instant::now() + Duration::from_secs(2);
            if let Ok(id) = en.get_default_device(dir).and_then(|d| d.get_id())
                && !dev_id.is_empty()
                && id != dev_id
            {
                break Err("default device changed — switching".into());
            }
        }
        // v1.3.1 FIX: whole frames only (see capture_ffmpeg).
        if bytes.len() >= 4 * AUD_CH {
            let n = bytes.len() / (4 * AUD_CH) * (4 * AUD_CH);
            samples.clear();
            let (a, b) = bytes.as_slices();
            let mut it = a.iter().chain(b.iter()).take(n).copied();
            while let (Some(b0), Some(b1), Some(b2), Some(b3)) =
                (it.next(), it.next(), it.next(), it.next())
            {
                samples.push(f32::from_le_bytes([b0, b1, b2, b3]));
            }
            bytes.drain(..n);
            lock(ring).push(&samples);
        }
    };
    let _ = client.stop_stream();
    res
}

/// Random, unguessable token (no RNG crate: OS-seeded SipHash keys).
fn rand_hex() -> String {
    use std::hash::{BuildHasher, Hasher};
    let part = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.finish()
    };
    format!("{:016x}{:016x}", part(), part())
}

/// Per-output transport for the mixed audio, created *before* ffmpeg so
/// its input open succeeds immediately.
///  * Windows: named pipe, owner-only DACL, single instance, no remote.
///  * Unix: Unix-domain socket inside a fresh 0700 directory.
#[cfg(windows)]
struct AudioPipe {
    name: String,
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: the HANDLE is a plain kernel object handle, used by one thread.
#[cfg(windows)]
unsafe impl Send for AudioPipe {}

#[cfg(windows)]
impl AudioPipe {
    fn create() -> Result<Self, String> {
        use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_OUTBOUND,
        };
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
        };
        let name = format!(r"\\.\pipe\litecast-{}-{}", std::process::id(), rand_hex());
        let wname: Vec<u16> = name.encode_utf16().chain([0]).collect();
        // Owner-only DACL: other local users cannot connect and read the mic.
        let sddl: Vec<u16> = "D:P(A;;GA;;;OW)(A;;GA;;;SY)"
            .encode_utf16()
            .chain([0])
            .collect();
        let mut sd = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated wide strings and out-pointers; the
        // descriptor is freed with LocalFree below as the API requires.
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(format!(
                    "security descriptor: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let h = CreateNamedPipeW(
                wname.as_ptr(),
                PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,          // exactly one client: the encoder
                256 * 1024, // ~0.7 s of audio
                0,
                0,
                &sa,
            );
            let err = std::io::Error::last_os_error();
            LocalFree(sd as _);
            if h == INVALID_HANDLE_VALUE || h.is_null() {
                return Err(format!("CreateNamedPipe: {err}"));
            }
            Ok(Self { name, handle: h })
        }
    }

    fn url(&self) -> String {
        format!("file:{}", self.name)
    }

    /// Unblock a ConnectNamedPipe that will never be satisfied (ffmpeg died
    /// before opening its input) by connecting to ourselves once.
    fn poke(name: &str) {
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
        let w: Vec<u16> = name
            .trim_start_matches("file:")
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: plain CreateFile/CloseHandle on a NUL-terminated name.
        unsafe {
            let h = CreateFileW(
                w.as_ptr(),
                GENERIC_READ,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if h != INVALID_HANDLE_VALUE {
                CloseHandle(h);
            }
        }
    }

    /// Wait for ffmpeg, then stream mixed audio until stop or ffmpeg exits.
    /// Closing the handle on return gives ffmpeg a clean EOF, so it can
    /// finalise the file.
    fn serve(self, rx: std::sync::mpsc::Receiver<AudioChunk>, stop: Arc<AtomicBool>) {
        use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError};
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
        // SAFETY: handle is a valid pipe owned by self for the whole call.
        let connected = unsafe {
            ConnectNamedPipe(self.handle, std::ptr::null_mut()) != 0
                || GetLastError() == ERROR_PIPE_CONNECTED
        };
        if !connected || stop.load(Relaxed) {
            return;
        }
        while !stop.load(Relaxed) {
            let c = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(c) => c,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            let mut off = 0;
            while off < c.len() {
                let mut n = 0u32;
                // SAFETY: in-bounds slice pointer/length; synchronous write.
                let ok = unsafe {
                    WriteFile(
                        self.handle,
                        c[off..].as_ptr(),
                        (c.len() - off) as u32,
                        &mut n,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || n == 0 {
                    return; // ffmpeg closed its end
                }
                off += n as usize;
            }
        }
    }
}

#[cfg(windows)]
impl Drop for AudioPipe {
    fn drop(&mut self) {
        // SAFETY: we own the handle; closed exactly once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(unix)]
struct AudioPipe {
    dir: PathBuf,
    path: PathBuf,
    listener: std::os::unix::net::UnixListener,
}

#[cfg(unix)]
impl AudioPipe {
    fn create() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        // Fresh 0700 directory (create fails if it exists: no symlink races);
        // only this user can reach the socket inside it.
        let dir = std::env::temp_dir().join(format!(
            "litecast-{}-{}",
            std::process::id(),
            &rand_hex()[..16]
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("audio socket dir: {e}"))?;
        let path = dir.join("a.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).map_err(|e| {
            let _ = fs::remove_dir(&dir);
            format!("audio socket: {e}")
        })?;
        let _ = listener.set_nonblocking(true);
        Ok(Self {
            dir,
            path,
            listener,
        })
    }

    fn url(&self) -> String {
        format!("unix:{}", self.path.display())
    }

    fn poke(_url: &str) {} // serve() polls; nothing can block forever

    fn serve(self, rx: std::sync::mpsc::Receiver<AudioChunk>, stop: Arc<AtomicBool>) {
        let t = Instant::now();
        let mut sock = loop {
            if stop.load(Relaxed) || t.elapsed() > Duration::from_secs(20) {
                return;
            }
            match self.listener.accept() {
                Ok((s, _)) => break s,
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        };
        let _ = sock.set_nonblocking(false);
        let _ = sock.set_write_timeout(Some(Duration::from_secs(5)));
        while !stop.load(Relaxed) {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(c) => {
                    if sock.write_all(&c).is_err() {
                        return; // ffmpeg went away (SIGPIPE is ignored by std)
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        let _ = sock.shutdown(std::net::Shutdown::Write); // clean EOF
    }
}

#[cfg(unix)]
impl Drop for AudioPipe {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.dir);
    }
}

/// Settings → capture sources: (label, is_mic, spec).
fn audio_sources(s: &Settings, ff: &FfInfo) -> Vec<(String, bool, CapSpec)> {
    let mut v = vec![];
    if s.audio_api == "none" {
        return v;
    }
    for (label, is_mic, dev) in [
        ("Mic", true, s.mic_device.trim()),
        ("Desktop", false, s.desktop_device.trim()),
    ] {
        if dev.is_empty() {
            continue;
        }
        let spec = if s.audio_api == "wasapi" {
            CapSpec::Wasapi {
                dev: dev.to_string(),
                loopback: !is_mic,
            }
        } else {
            match audio_capture_args(s, ff, dev) {
                Some(args) => CapSpec::Ffmpeg {
                    prog: s.ffmpeg_path.clone(),
                    args,
                },
                None => continue,
            }
        };
        v.push((label.to_string(), is_mic, spec));
    }
    v
}

/// argv for an ffmpeg capture child: device → 48 kHz stereo f32le on stdout.
fn audio_capture_args(s: &Settings, ff: &FfInfo, dev: &str) -> Option<Vec<String>> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
        .iter()
        .map(|x| x.to_string())
        .collect();
    if ff.input_queue_opt() {
        a.extend(["-thread_queue_size".into(), "1024".into()]);
    }
    let input: Vec<String> = match s.audio_api.as_str() {
        "pulse" => vec!["-f".into(), "pulse".into(), "-i".into(), dev.into()],
        "alsa" => vec!["-f".into(), "alsa".into(), "-i".into(), dev.into()],
        "dshow" => vec![
            "-f".into(),
            "dshow".into(),
            "-audio_buffer_size".into(),
            "50".into(),
            "-i".into(),
            format!("audio={dev}"),
        ],
        "avfoundation" => vec![
            "-f".into(),
            "avfoundation".into(),
            "-i".into(),
            format!(":{dev}"),
        ],
        // Test-only synthetic source (CI): never offered in the UI, and
        // rejected by sanitize() unless LITECAST_TEST_AUDIO=1.
        "lavfi" => vec![
            "-re".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            dev.into(),
        ],
        _ => return None,
    };
    a.extend(input);
    a.extend(
        ["-vn", "-f", "f32le", "-ar", "48000", "-ac", "2", "pipe:1"]
            .iter()
            .map(|x| x.to_string()),
    );
    Some(a)
}

/// Runs on its own thread: initialising COM as MTA on the FLTK UI thread
/// would break its native (STA/OLE) file dialogs.
#[cfg(windows)]
fn wasapi_devices() -> DevList {
    thread::spawn(wasapi_devices_mta).join().unwrap_or_default()
}

#[cfg(windows)]
fn wasapi_devices_mta() -> DevList {
    use wasapi::{DeviceEnumerator, Direction};
    let _ = wasapi::initialize_mta();
    let mut v: DevList = vec![
        ("default".into(), "default — microphone / speakers".into()),
        (String::new(), "(off)".into()),
    ];
    if let Ok(en) = DeviceEnumerator::new() {
        for (dir, tag) in [(Direction::Capture, "in"), (Direction::Render, "out")] {
            if let Ok(coll) = en.get_device_collection(dir) {
                for d in (&coll).into_iter().flatten() {
                    if let Ok(n) = d.get_friendlyname() {
                        v.push((n.clone(), format!("[{tag}] {n}")));
                    }
                }
            }
        }
    }
    v
}

#[cfg(not(windows))]
fn wasapi_devices() -> DevList {
    vec![]
}

/// Horizontal VU strip renderer (−60…0 dBFS): RMS bar in green/yellow/red
/// zones, peak line, peak-hold tick, target marker, readout text.
fn draw_meter_row(x: i32, y: i32, w: i32, h: i32, m: &ChanMeter, target: f32, live: bool) {
    use draw::*;
    let lw = 54; // label column
    let tw = 92; // readout column
    let bx = x + lw;
    let bw = (w - lw - tw).max(20);
    let pos = |d: f32| bx + ((d.clamp(-60.0, 0.0) + 60.0) / 60.0 * bw as f32) as i32;
    set_draw_color(Color::from_rgb(200, 200, 205));
    set_font(Font::Helvetica, 11);
    draw_text2(&m.label, x, y, lw - 4, h, Align::Left | Align::Inside);
    // Activity LED: gate open / signal above the auto-level floor.
    if live && m.active && !m.muted {
        draw_rect_fill(bx - 8, y + h / 2 - 2, 5, 5, Color::from_rgb(80, 220, 110));
    }
    draw_rect_fill(bx, y + 3, bw, h - 6, Color::from_rgb(28, 28, 32));
    if live && !m.muted {
        // Zones: ≤ −20 green, −20…−9 yellow, > −9 red.
        let zones = [
            (-60.0, -20.0, Color::from_rgb(60, 190, 90)),
            (-20.0, -9.0, Color::from_rgb(225, 200, 60)),
            (-9.0, 0.0, Color::from_rgb(230, 70, 60)),
        ];
        for (a, b, c) in zones {
            let hi = m.rms.min(b);
            if hi > a {
                let (x0, x1) = (pos(a), pos(hi));
                draw_rect_fill(x0, y + 4, (x1 - x0).max(1), h - 8, c);
            }
        }
        set_draw_color(Color::from_rgb(235, 235, 240));
        let px = pos(m.peak);
        draw_line(px, y + 4, px, y + h - 5);
        if m.hold > -60.0 {
            set_draw_color(if m.hold > -1.0 {
                Color::from_rgb(255, 60, 60)
            } else {
                Color::White
            });
            let hx = pos(m.hold);
            draw_rect_fill(hx - 1, y + 3, 2, h - 6, get_color());
        }
    }
    // Target marker (where auto-level aims RMS).
    set_draw_color(Color::from_rgb(110, 160, 255));
    let tx = pos(target);
    draw_line(tx, y + 2, tx, y + h - 3);
    // Readout.
    set_draw_color(Color::from_rgb(200, 200, 205));
    let txt = if !live {
        "idle".to_string()
    } else if m.muted {
        "muted".to_string()
    } else {
        let mut t = format!("{:>4.0} dB", m.rms.max(-99.0));
        if m.gr >= 0.5 {
            t.push_str(&format!(" GR{:.0}", m.gr));
        }
        if m.agc.abs() >= 0.5 && m.label != "Master" {
            t.push_str(&format!(" {:+.0}", m.agc));
        }
        t
    };
    draw_text2(&txt, bx + bw + 4, y, tw - 4, h, Align::Left | Align::Inside);
}

struct OutputSession {
    kind: OutKind,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
    drain: Option<JoinHandle<()>>,
    started: Instant,
    frames: Arc<AtomicU64>,
    last_err: Arc<Mutex<String>>,
    label: String,
    /// Stream only: (name, url, last error time) per destination.
    health: DestHealth,
    /// Native audio: pipe-feeding thread, its ffmpeg URL, and the hub.
    audio_writer: Option<JoinHandle<()>>,
    audio_pipe: Option<String>,
    _hub: Option<Arc<AudioHub>>,
    /// v1.3.2: audio blocks this output lost (10 ms each).
    audio_drops: Option<Arc<AtomicU64>>,
}

impl OutputSession {
    #[allow(clippy::too_many_arguments)] // one call site per output kind
    fn start(
        kind: OutKind,
        s: &Settings,
        ff: &FfInfo,
        comp: &Arc<CompShared>,
        log: &Logger,
        targets: &[(String, String)], // (display name, path or ingest URL)
        label: String,
        hub: Option<Arc<AudioHub>>,
    ) -> Result<Self, String> {
        let urls: Vec<String> = targets.iter().map(|t| t.1.clone()).collect();
        // The pipe must exist before ffmpeg opens its inputs.
        let pipe = match &hub {
            Some(_) => Some(AudioPipe::create()?),
            None => None,
        };
        let pipe_name: Option<String> = pipe.as_ref().map(AudioPipe::url);
        let args = encoder_args(s, ff, kind, &urls, pipe_name.as_deref());
        let mut cmd = ffmpeg_cmd(&s.ffmpeg_path);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start ffmpeg ({}): {e}", s.ffmpeg_path))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let tag = if kind == OutKind::Record {
            "rec"
        } else {
            "stream"
        };
        let last_err = Arc::new(Mutex::new(String::new()));
        let health: DestHealth = Arc::new(Mutex::new(
            targets
                .iter()
                .map(|(n, u)| (n.clone(), u.clone(), None))
                .collect(),
        ));
        // Per-destination health: any ffmpeg error line naming a destination's
        // URL marks it as reconnecting for the next 10 s.
        let hook: Option<LineHook> = (kind == OutKind::Stream).then(|| {
            let h = health.clone();
            Box::new(move |line: &str| {
                for d in lock(&h).iter_mut() {
                    if line.contains(d.1.as_str()) {
                        d.2 = Some(Instant::now());
                    }
                }
            }) as LineHook
        });
        let drain = child.stderr.take().and_then(|e| {
            spawn_stderr_drain_hook(e, tag.into(), log.clone(), last_err.clone(), hook)
        });
        let sess = Self {
            kind,
            child: Arc::new(Mutex::new(Some(child))),
            stop: Arc::new(AtomicBool::new(false)),
            dead: Arc::new(AtomicBool::new(false)),
            writer: None,
            drain,
            started: Instant::now(),
            frames: Arc::new(AtomicU64::new(0)),
            last_err,
            label,
            health,
            audio_writer: None,
            audio_pipe: pipe_name,
            _hub: hub.clone(),
            audio_drops: None,
        };
        let (stop, dead, frames, comp2, log2) = (
            sess.stop.clone(),
            sess.dead.clone(),
            sess.frames.clone(),
            comp.clone(),
            log.clone(),
        );
        let mut sess = sess;
        sess.writer = Some(
            thread::Builder::new()
                .name(format!("writer-{tag}"))
                .spawn(move || writer_loop(stdin, comp2, stop, dead, frames, log2, tag))
                .map_err(|e| e.to_string())?,
        );
        if let (Some(p), Some(h)) = (pipe, &hub) {
            let ((rx, drops), stop) = (h.subscribe(tag), sess.stop.clone());
            sess.audio_drops = Some(drops);
            sess.audio_writer = thread::Builder::new()
                .name(format!("audio-{tag}"))
                .spawn(move || p.serve(rx, stop))
                .ok();
        }
        log.log(format!("[{tag}] started → {}", sess.label));
        Ok(sess)
    }

    /// "Kick ✓, Rumble ⟳" — ⟳ = error seen in the last 10 s (fifo reconnecting).
    fn health_summary(&self) -> String {
        lock(&self.health)
            .iter()
            .map(|(n, _, t)| {
                let bad = t
                    .map(|t| t.elapsed() < Duration::from_secs(10))
                    .unwrap_or(false);
                format!("{n} {}", if bad { "⟳" } else { "✓" })
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// v1.3.2: status-bar suffix when this output has lost audio.
    fn drop_note(&self) -> String {
        match self.audio_drops.as_ref().map(|d| d.load(Relaxed)) {
            Some(n) if n > 0 => format!(" ⚠ audio −{} ms", n * 10),
            _ => String::new(),
        }
    }

    fn alive(&self) -> bool {
        if self.dead.load(Relaxed) {
            return false;
        }
        match lock(&self.child).as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Graceful stop off the UI thread: close stdin so ffmpeg flushes and
    /// finalises the container (critical for mp4 moov atom), then escalate.
    fn begin_stop(mut self, log: Logger) -> Option<JoinHandle<()>> {
        thread::Builder::new()
            .name("stopper".into())
            .spawn(move || {
                self.stop.store(true, Relaxed);
                let tag = if self.kind == OutKind::Record {
                    "rec"
                } else {
                    "stream"
                };
                if let Some(w) = self.writer.take() {
                    let t = Instant::now();
                    while !w.is_finished() && t.elapsed() < Duration::from_secs(3) {
                        thread::sleep(Duration::from_millis(20));
                    }
                    if !w.is_finished()
                        && let Some(c) = lock(&self.child).as_mut()
                    {
                        let _ = c.kill(); // writer blocked on a stalled encoder
                    }
                    let _ = w.join();
                }
                // Audio pipe: exits within 200 ms of `stop`; if ffmpeg never
                // opened it, ConnectNamedPipe is still blocked — poke it.
                if let Some(a) = self.audio_writer.take() {
                    let t = Instant::now();
                    while !a.is_finished() && t.elapsed() < Duration::from_secs(2) {
                        thread::sleep(Duration::from_millis(20));
                    }
                    if !a.is_finished()
                        && let Some(n) = &self.audio_pipe
                    {
                        AudioPipe::poke(n);
                    }
                    // v1.3.1 FIX: the Windows pipe write has no timeout; if the
                    // encoder stopped reading audio, the writer blocked forever
                    // and join() hung this thread (encoder never reaped).
                    // Killing the encoder breaks the pipe and frees it.
                    let t = Instant::now();
                    while !a.is_finished() && t.elapsed() < Duration::from_secs(2) {
                        thread::sleep(Duration::from_millis(20));
                    }
                    if !a.is_finished()
                        && let Some(c) = lock(&self.child).as_mut()
                    {
                        let _ = c.kill();
                    }
                    let _ = a.join();
                }
                let mut graceful = false;
                if let Some(mut c) = lock(&self.child).take() {
                    let t = Instant::now();
                    loop {
                        match c.try_wait() {
                            Ok(Some(_)) => {
                                graceful = true;
                                break;
                            }
                            Ok(None) if t.elapsed() < Duration::from_secs(15) => {
                                thread::sleep(Duration::from_millis(50))
                            }
                            _ => {
                                let _ = c.kill();
                                let _ = c.wait();
                                break;
                            }
                        }
                    }
                }
                if let Some(d) = self.drain.take() {
                    let _ = d.join();
                }
                log.log(format!(
                    "[{tag}] stopped ({} frames, {}){}",
                    self.frames.load(Relaxed),
                    fmt_dur(self.started.elapsed()),
                    if graceful {
                        ""
                    } else {
                        " — encoder had to be killed"
                    }
                ));
            })
            .ok()
    }
}

fn writer_loop(
    mut stdin: ChildStdin,
    comp: Arc<CompShared>,
    stop: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    log: Logger,
    tag: &str,
) {
    let period_ns = 1_000_000_000u64 / comp.fps.max(1) as u64;
    let mut t0 = Instant::now();
    let mut n: u64 = 0;
    while !stop.load(Relaxed) {
        let f = lock(&comp.canvas).clone();
        if let Some(f) = f {
            if let Err(e) = stdin.write_all(&f) {
                log.log(format!("[{tag}] encoder pipe closed: {e}"));
                dead.store(true, Relaxed);
                break;
            }
            frames.fetch_add(1, Relaxed);
        }
        n += 1;
        let target = t0 + Duration::from_nanos(period_ns.saturating_mul(n));
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        } else if now - target > Duration::from_nanos(period_ns * 3) {
            // Encoder can't keep up; wallclock timestamps + cfr keep A/V in sync.
            t0 = now;
            n = 0;
        }
    }
    drop(stdin); // EOF → ffmpeg finalises
}

// ----------------------------------------------------------------------------
// Time helpers
// ----------------------------------------------------------------------------
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// UTC timestamp for filenames (no chrono dependency).
fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let p = dir.join(format!("{stem}.{ext}"));
    if !p.exists() {
        return p;
    }
    (1..1000)
        .map(|i| dir.join(format!("{stem}_{i}.{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(p)
}

fn fit_rect(cw: u32, ch: u32, fw: i32, fh: i32) -> (i32, i32, i32, i32) {
    if cw == 0 || ch == 0 || fw <= 0 || fh <= 0 {
        return (0, 0, 0, 0);
    }
    let s = (fw as f64 / cw as f64).min(fh as f64 / ch as f64);
    let w = ((cw as f64 * s).floor() as i32).max(1);
    let h = ((ch as f64 * s).floor() as i32).max(1);
    ((fw - w) / 2, (fh - h) / 2, w, h)
}

/// FLTK widget labels treat '@' as a symbol prefix.
fn label_escape(s: &str) -> String {
    s.replace('@', "@@")
}

/// FLTK menu labels treat / & _ \ specially.
fn menu_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        match c {
            '/' => o.push_str("\\/"),
            '@' => o.push_str("@@"),
            '\\' => o.push_str("\\\\"),
            '&' => o.push_str("&&"),
            '_' if i == 0 => o.push_str("\\_"),
            '|' => o.push(' '),
            _ => o.push(c),
        }
    }
    o
}

// ----------------------------------------------------------------------------
// UI messages / queue
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq)]
enum Msg {
    Tick,
    Quit,
    SceneSelect,
    SceneAdd,
    SceneRemove,
    SceneRename,
    SceneUp,
    SceneDown,
    SourceSelect,
    SourceAdd,
    SourceEdit,
    SourceRemove,
    SourceToggle,
    SourceUp,
    SourceDown,
    PropsApply,
    PropsFill,
    PropsCenter,
    PropsNative,
    PropsOpacity,
    ToggleRecord,
    ToggleStream,
    Screenshot,
    Settings,
    Destinations,
    AudioMixer,
    SaveProfile,
    SaveProfileAs,
    LoadProfile,
    About,
    PreviewPush(i32, i32, bool),
    PreviewDrag(i32, i32),
    PreviewRelease,
}

#[derive(Clone)]
struct Q(Rc<RefCell<VecDeque<Msg>>>);
impl Q {
    fn send(&self, m: Msg) {
        let mut q = self.0.borrow_mut();
        if m == Msg::Tick && q.back() == Some(&Msg::Tick) {
            return; // coalesce ticks while a modal dialog runs
        }
        if q.len() < 10_000 {
            q.push_back(m);
        }
    }
    fn pop(&self) -> Option<Msg> {
        self.0.borrow_mut().pop_front()
    }
    fn cb<W>(&self, m: Msg) -> impl FnMut(&mut W) + 'static {
        let q = self.clone();
        move |_| q.send(m.clone())
    }
}

#[derive(Default)]
struct Overlay {
    cw: u32,
    ch: u32,
    sel: Option<(i32, i32, i32, i32)>,
}

struct Ui {
    win: Window,
    preview: Frame,
    scenes: HoldBrowser,
    sources: HoldBrowser,
    px: IntInput,
    py: IntInput,
    pw: IntInput,
    ph: IntInput,
    opac: HorValueSlider,
    btn_rec: Button,
    btn_stream: Button,
    status: Frame,
    src_status: Frame,
    logbuf: TextBuffer,
    logview: TextDisplay,
    /// v1.3.3: section headers, kept so a theme change can recolour them.
    headers: Vec<Frame>,
}

// ----------------------------------------------------------------------------
// Theme (v1.3.3): one palette for the whole window. Dark matches the preview
// and meter panels, which were always dark; Light is the classic FLTK look.
// ----------------------------------------------------------------------------
static UI_DARK: AtomicBool = AtomicBool::new(true);

fn ui_dark() -> bool {
    UI_DARK.load(Relaxed)
}

/// Desktop light/dark preference; None if the desktop does not say.
#[cfg(windows)]
fn system_prefers_dark() -> Option<bool> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (key, val) = (
        w(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
        w("AppsUseLightTheme"),
    );
    let (mut data, mut size) = (0u32, 4u32);
    // SAFETY: NUL-terminated wide strings; out-buffer is a u32 of `size` bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (rc == 0).then_some(data == 0) // 0 = apps use dark
}

#[cfg(target_os = "macos")]
fn system_prefers_dark() -> Option<bool> {
    let a = ["read", "-g", "AppleInterfaceStyle"].map(String::from);
    // The key only exists (= "Dark") in dark mode.
    run_tool("defaults", &a, None, 256, Duration::from_secs(2), None)
        .ok()
        .map(|(o, _, code)| code == Some(0) && String::from_utf8_lossy(&o).contains("Dark"))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_prefers_dark() -> Option<bool> {
    if let Some(t) = std::env::var_os("GTK_THEME") {
        return Some(t.to_string_lossy().to_ascii_lowercase().contains("dark"));
    }
    let a = ["get", "org.gnome.desktop.interface", "color-scheme"].map(String::from);
    let (o, _, code) = run_tool("gsettings", &a, None, 256, Duration::from_secs(2), None).ok()?;
    let v = String::from_utf8_lossy(&o).to_ascii_lowercase();
    (code == Some(0)).then(|| v.contains("dark"))
}

/// Resolve a theme name to dark/light. `system` is only asked for "system".
fn theme_is_dark(theme: &str, system: impl FnOnce() -> Option<bool>) -> bool {
    match theme {
        "light" => false,
        "system" => system().unwrap_or(true),
        _ => true,
    }
}

/// Set the global FLTK palette for `theme`; returns true if it is dark.
/// Unknown "system" answers fall back to dark (the app default).
fn apply_theme(theme: &str) -> bool {
    let dark = theme_is_dark(theme, system_prefers_dark);
    UI_DARK.store(dark, Relaxed);
    if dark {
        app::background(38, 39, 44); // panels, buttons
        app::background2(26, 27, 31); // lists, inputs, log
        app::foreground(222, 224, 230); // text
        app::set_selection_color(46, 110, 200); // accent
        app::set_inactive_color(110, 112, 120);
    } else {
        // Classic FLTK/GTK-scheme defaults (the pre-1.3.3 look), written as
        // literals: Color::to_rgb() reads FLTK's static map, not live values.
        app::background(192, 192, 192);
        app::background2(255, 255, 255);
        app::foreground(0, 0, 0);
        app::set_selection_color(0, 0, 128);
        app::set_inactive_color(85, 85, 85);
    }
    app::set_visible_focus(false); // no dotted focus boxes in either theme
    dark
}

/// Per-widget colours that the global palette does not cover.
struct Pal;
impl Pal {
    fn header() -> Color {
        if ui_dark() {
            Color::from_rgb(140, 146, 160)
        } else {
            Color::from_rgb(55, 58, 66)
        }
    }
    fn status_bg() -> Color {
        if ui_dark() {
            Color::from_rgb(22, 23, 27)
        } else {
            Color::from_rgb(40, 40, 46)
        }
    }
    fn log_text() -> Color {
        if ui_dark() {
            Color::from_rgb(190, 196, 206)
        } else {
            Color::Black
        }
    }
    /// Idle Start Recording / Start Streaming: tinted so the two primary
    /// actions stand out before they are pressed.
    fn idle_rec() -> Color {
        if ui_dark() {
            Color::from_rgb(70, 40, 44)
        } else {
            Color::from_rgb(222, 190, 190)
        }
    }
    fn idle_live() -> Color {
        if ui_dark() {
            Color::from_rgb(36, 52, 78)
        } else {
            Color::from_rgb(190, 205, 228)
        }
    }
}

fn col_header(t: &str) -> Frame {
    // v1.3.3: small, muted, upper-case section headers.
    let mut f = Frame::default().with_label(&t.to_uppercase());
    f.set_label_font(Font::HelveticaBold);
    f.set_label_size(11);
    f.set_label_color(Pal::header());
    f.set_align(Align::Left | Align::Inside);
    f
}

fn small_btn(l: &str, tip: &str) -> Button {
    let mut b = Button::default().with_label(l);
    b.set_tooltip(tip);
    b.set_label_size(12);
    b
}

fn build_ui(
    q: &Q,
    pv: Arc<PreviewShared>,
    ov: Arc<Mutex<Overlay>>,
    meters: Arc<Mutex<Meters>>,
) -> Ui {
    let mut win = Window::default()
        .with_size(1280, 820)
        .with_label(&format!("{APP_NAME} v{APP_VERSION}"));
    win.size_range(900, 600, 0, 0);
    let mut main = Flex::default_fill().column();
    main.set_margin(0);
    main.set_pad(0);

    // Menu --------------------------------------------------------------------
    let mut mb = MenuBar::default();
    let add = |mb: &mut MenuBar, path: &str, sc: Shortcut, m: Msg| {
        let q = q.clone();
        mb.add(path, sc, MenuFlag::Normal, move |_| q.send(m.clone()));
    };
    add(
        &mut mb,
        "&File/&Save Profile",
        Shortcut::Ctrl | 's',
        Msg::SaveProfile,
    );
    add(
        &mut mb,
        "&File/Save Profile &As…",
        Shortcut::Ctrl | Shortcut::Shift | 's',
        Msg::SaveProfileAs,
    );
    add(
        &mut mb,
        "&File/&Load Profile…",
        Shortcut::Ctrl | 'o',
        Msg::LoadProfile,
    );
    add(&mut mb, "&File/&Quit", Shortcut::Ctrl | 'q', Msg::Quit);
    add(
        &mut mb,
        "&Controls/Toggle &Recording",
        Shortcut::Ctrl | 'r',
        Msg::ToggleRecord,
    );
    add(
        &mut mb,
        "&Controls/Toggle &Streaming",
        Shortcut::Ctrl | 'l',
        Msg::ToggleStream,
    );
    add(
        &mut mb,
        "&Controls/Screens&hot",
        Shortcut::None | Key::F12,
        Msg::Screenshot,
    );
    add(
        &mut mb,
        "&Controls/S&ettings…",
        Shortcut::Ctrl | ',',
        Msg::Settings,
    );
    add(
        &mut mb,
        "&Controls/Stream &Destinations…",
        Shortcut::Ctrl | 'd',
        Msg::Destinations,
    );
    add(
        &mut mb,
        "&Controls/&Audio Mixer…",
        Shortcut::Ctrl | 'm',
        Msg::AudioMixer,
    );
    add(&mut mb, "&Help/&About", Shortcut::None, Msg::About);
    main.fixed(&mb, 26);

    // Preview -----------------------------------------------------------------
    let mut preview = Frame::default();
    preview.set_frame(FrameType::FlatBox);
    preview.set_color(Color::from_rgb(22, 22, 26));
    {
        let (pv, ov) = (pv.clone(), ov.clone());
        preview.draw(move |f| {
            let (fx, fy, fw, fh) = (f.x(), f.y(), f.w(), f.h());
            draw::draw_rect_fill(fx, fy, fw, fh, Color::from_rgb(22, 22, 26));
            let o = lock(&ov);
            let (ox, oy, pw, ph) = fit_rect(o.cw, o.ch, fw, fh);
            if pw <= 0 {
                return;
            }
            {
                let g = lock(&pv.buf);
                // v1.3.2: tell the compositor this frame reached the screen.
                pv.drawn.store(pv.seq.load(Relaxed), Relaxed);
                if g.1 > 0 && g.2 > 0 && g.0.len() >= g.1 * g.2 * 3 {
                    if let Ok(mut img) = RgbImage::new(
                        &g.0[..g.1 * g.2 * 3],
                        g.1 as i32,
                        g.2 as i32,
                        ColorDepth::Rgb8,
                    ) {
                        if g.1 as i32 != pw || g.2 as i32 != ph {
                            img.scale(pw, ph, false, true);
                        }
                        img.draw(fx + ox, fy + oy, pw, ph);
                    }
                } else {
                    draw::draw_rect_fill(fx + ox, fy + oy, pw, ph, Color::Black);
                }
            }
            if let Some((x, y, w, h)) = o.sel {
                let sx = |v: i32| fx + ox + (v as f64 * pw as f64 / o.cw as f64).round() as i32;
                let sy = |v: i32| fy + oy + (v as f64 * ph as f64 / o.ch as f64).round() as i32;
                let (x0, y0, x1, y1) = (sx(x), sy(y), sx(x + w), sy(y + h));
                draw::push_clip(fx, fy, fw, fh);
                draw::set_draw_color(Color::from_rgb(255, 64, 64));
                draw::set_line_style(draw::LineStyle::Solid, 2);
                draw::draw_rect(x0, y0, (x1 - x0).max(2), (y1 - y0).max(2));
                draw::draw_rect_fill(x1 - 5, y1 - 5, 10, 10, Color::from_rgb(255, 64, 64));
                draw::set_line_style(draw::LineStyle::Solid, 0);
                draw::pop_clip();
            }
        });
    }
    {
        let q = q.clone();
        preview.handle(move |f, ev| {
            let (mx, my) = (app::event_x() - f.x(), app::event_y() - f.y());
            match ev {
                Event::Push if app::event_mouse_button() == app::MouseButton::Left => {
                    q.send(Msg::PreviewPush(mx, my, app::is_event_shift()));
                    true
                }
                Event::Drag => {
                    q.send(Msg::PreviewDrag(mx, my));
                    true
                }
                Event::Released => {
                    q.send(Msg::PreviewRelease);
                    true
                }
                _ => false,
            }
        });
    }

    // Dock --------------------------------------------------------------------
    let mut dock = Flex::default().row();
    dock.set_margin(6);
    dock.set_pad(8);

    // Scenes column
    let mut c1 = Flex::default().column();
    c1.set_pad(3);
    let h1 = col_header("Scenes");
    c1.fixed(&h1, 20);
    let mut scenes = HoldBrowser::default();
    scenes.set_callback(q.cb(Msg::SceneSelect));
    let mut r1 = Flex::default().row();
    r1.set_pad(2);
    small_btn("+", "Add scene").set_callback(q.cb(Msg::SceneAdd));
    small_btn("−", "Remove scene").set_callback(q.cb(Msg::SceneRemove));
    small_btn("Ren", "Rename scene").set_callback(q.cb(Msg::SceneRename));
    small_btn("@8>", "Move up").set_callback(q.cb(Msg::SceneUp));
    small_btn("@2>", "Move down").set_callback(q.cb(Msg::SceneDown));
    r1.end();
    c1.fixed(&r1, 26);
    c1.end();
    dock.fixed(&c1, 165); // v1.3.3: was 190

    // Sources column
    let mut c2 = Flex::default().column();
    c2.set_pad(3);
    let h2 = col_header("Sources");
    c2.fixed(&h2, 20);
    let mut sources = HoldBrowser::default();
    {
        let q = q.clone();
        sources.set_callback(move |_| {
            q.send(Msg::SourceSelect);
            if app::event_clicks() {
                q.send(Msg::SourceEdit);
            }
        });
    }
    let mut r2 = Flex::default().row();
    r2.set_pad(2);
    small_btn("+", "Add source").set_callback(q.cb(Msg::SourceAdd));
    small_btn("−", "Remove source").set_callback(q.cb(Msg::SourceRemove));
    small_btn("Edit", "Edit source").set_callback(q.cb(Msg::SourceEdit));
    small_btn("Eye", "Show/hide").set_callback(q.cb(Msg::SourceToggle));
    small_btn("@8>", "Raise").set_callback(q.cb(Msg::SourceUp));
    small_btn("@2>", "Lower").set_callback(q.cb(Msg::SourceDown));
    r2.end();
    c2.fixed(&r2, 26);
    c2.end();
    dock.fixed(&c2, 225); // v1.3.3: was 270

    // Transform column
    let mut c3 = Flex::default().column();
    c3.set_pad(4);
    let h3 = col_header("Transform");
    c3.fixed(&h3, 20);
    let mk_row = |c: &mut Flex, l1: &str, l2: &str| -> (IntInput, IntInput) {
        let mut r = Flex::default().row();
        r.set_pad(4);
        let f1 = Frame::default().with_label(l1);
        r.fixed(&f1, 16);
        let a = IntInput::default();
        let f2 = Frame::default().with_label(l2);
        r.fixed(&f2, 16);
        let b = IntInput::default();
        r.end();
        c.fixed(&r, 26);
        (a, b)
    };
    let (px, py) = mk_row(&mut c3, "X", "Y");
    let (pw, ph) = mk_row(&mut c3, "W", "H");
    let mut opac = HorValueSlider::default().with_label("Opacity %");
    opac.set_align(Align::Bottom);
    opac.set_range(0.0, 100.0);
    opac.set_step(1.0, 1);
    opac.set_value(100.0);
    opac.set_callback(q.cb(Msg::PropsOpacity));
    c3.fixed(&opac, 22);
    let sp = Frame::default();
    c3.fixed(&sp, 16);
    let mut r3 = Flex::default().row();
    r3.set_pad(2);
    small_btn("Apply", "Apply X/Y/W/H").set_callback(q.cb(Msg::PropsApply));
    small_btn("Fill", "Fit to canvas, keep aspect").set_callback(q.cb(Msg::PropsFill));
    small_btn("Center", "Center on canvas").set_callback(q.cb(Msg::PropsCenter));
    small_btn("1:1", "Native capture size").set_callback(q.cb(Msg::PropsNative));
    r3.end();
    c3.fixed(&r3, 26);
    let mut src_status = Frame::default();
    src_status.set_align(Align::Left | Align::Inside | Align::Wrap | Align::Top);
    src_status.set_label_size(11);
    c3.end();
    dock.fixed(&c3, 235); // v1.3.3: was 250

    // Controls column
    let mut c4 = Flex::default().column();
    c4.set_pad(6);
    let h4 = col_header("Controls");
    c4.fixed(&h4, 20);
    let mut btn_stream = Button::default().with_label("Start Streaming");
    btn_stream.set_callback(q.cb(Msg::ToggleStream));
    c4.fixed(&btn_stream, 34);
    let mut btn_rec = Button::default().with_label("Start Recording");
    btn_rec.set_callback(q.cb(Msg::ToggleRecord));
    c4.fixed(&btn_rec, 34);
    let mut b_shot = Button::default().with_label("Screenshot");
    b_shot.set_callback(q.cb(Msg::Screenshot));
    c4.fixed(&b_shot, 28);
    let mut b_set = Button::default().with_label("Settings…");
    b_set.set_callback(q.cb(Msg::Settings));
    c4.fixed(&b_set, 28);
    Frame::default();
    c4.end();
    dock.fixed(&c4, 160); // v1.3.3: was 170

    // Audio column: live VU meters (post-processing) + mixer button.
    let mut ca = Flex::default().column();
    ca.set_pad(3);
    let ha = col_header("Audio");
    ca.fixed(&ha, 20);
    let mut vu = Frame::default();
    vu.set_frame(FrameType::FlatBox);
    vu.set_color(Color::from_rgb(26, 27, 31));
    {
        let meters = meters.clone();
        vu.draw(move |f| {
            let (x, y, w, h) = (f.x(), f.y(), f.w(), f.h());
            draw::draw_rect_fill(x, y, w, h, Color::from_rgb(26, 27, 31));
            let m = lock(&meters).clone();
            let live = m
                .updated
                .map(|t| t.elapsed() < Duration::from_millis(500))
                .unwrap_or(false);
            let mut rows: Vec<ChanMeter> = m.chans.clone();
            if rows.is_empty() {
                draw::set_draw_color(Color::from_rgb(160, 160, 165));
                draw::set_font(Font::Helvetica, 11);
                draw::draw_text2(
                    "No audio sources\n(Settings → audio)",
                    x,
                    y,
                    w,
                    h,
                    Align::Center | Align::Inside,
                );
                return;
            }
            rows.push(m.master.clone());
            let rh = ((h - 8) / rows.len() as i32).clamp(16, 30);
            for (i, r) in rows.iter().enumerate() {
                draw_meter_row(
                    x + 4,
                    y + 4 + i as i32 * rh,
                    w - 8,
                    rh,
                    r,
                    m.target_db,
                    live,
                );
            }
        });
    }
    let mut b_mix = Button::default().with_label("Mixer…");
    b_mix.set_tooltip("Compressor, noise gate, auto-level, ducking, limiter");
    b_mix.set_callback(q.cb(Msg::AudioMixer));
    ca.fixed(&b_mix, 28);
    ca.end();
    dock.fixed(&ca, 265); // v1.3.3 FIX: was 300 — Log was squeezed to ~45 px
    // ~20 Hz meter refresh, only while the window is visible.
    {
        let mut vu = vu.clone();
        app::add_timeout3(0.05, move |h| {
            if vu.visible_r() {
                vu.redraw();
            }
            app::repeat_timeout3(0.05, h);
        });
    }

    // Log column
    let mut c5 = Flex::default().column();
    c5.set_pad(3);
    let h5 = col_header("Log");
    c5.fixed(&h5, 20);
    let logbuf = TextBuffer::default();
    let mut logview = TextDisplay::default();
    logview.set_buffer(logbuf.clone());
    logview.set_text_size(11);
    logview.set_text_font(Font::Courier);
    logview.set_text_color(Pal::log_text());
    logview.wrap_mode(fltk::text::WrapMode::AtBounds, 0); // v1.3.3: wrap, no sideways scrolling
    c5.end();
    dock.end();
    main.fixed(&dock, 236);

    let mut status = Frame::default();
    status.set_frame(FrameType::FlatBox);
    status.set_color(Pal::status_bg());
    status.set_label_color(Color::from_rgb(220, 220, 220));
    status.set_align(Align::Left | Align::Inside);
    status.set_label_size(12);
    main.fixed(&status, 22);
    main.end();
    win.end();
    win.resizable(&main);
    {
        let q = q.clone();
        win.set_callback(move |_| {
            // Escape also fires the window callback; only a real close quits.
            if app::event() == Event::Close {
                q.send(Msg::Quit);
            }
        });
    }
    Ui {
        win,
        preview,
        scenes,
        sources,
        px,
        py,
        pw,
        ph,
        opac,
        btn_rec,
        btn_stream,
        status,
        src_status,
        logbuf,
        logview,
        headers: vec![h1, h2, h3, h4, ha, h5],
    }
}

// ----------------------------------------------------------------------------
// Dialogs
// ----------------------------------------------------------------------------
fn modal_run(win: &mut Window) {
    win.make_modal(true);
    win.show();
    while win.shown() {
        app::wait();
    }
}

fn fill_choice(c: &mut Choice, items: &[(String, String)]) {
    c.clear();
    for (_, l) in items {
        c.add(&menu_escape(l), Shortcut::None, MenuFlag::Normal, |_| {});
    }
}

/// Add/Edit source dialog. Returns (name, kind) on OK.
fn source_dialog(init: Option<&Source>, st: &Settings) -> Option<(String, SourceKind)> {
    let mut win = Window::default()
        .with_size(580, 440)
        .center_screen()
        .with_label(if init.is_some() {
            "Edit Source"
        } else {
            "Add Source"
        });
    let mut name = Input::new(170, 14, 390, 26, "Name:");
    let mut kind = Choice::new(170, 48, 390, 26, "Type:");
    kind.add_choice("Display Capture|Webcam \\/ Video Device|Media File|Image|Color");
    let mut target = Input::new(170, 82, 290, 26, "Display / Device / Path:");
    let mut browse = Button::new(466, 82, 94, 26, "Browse…");
    let mut detected = Choice::new(170, 116, 290, 26, "Detected:");
    let mut detect_btn = Button::new(466, 116, 94, 26, "Detect");
    let mut monitor = Choice::new(170, 150, 390, 26, "Monitor:");
    let mut rx = IntInput::new(170, 184, 80, 26, "Region X / Y / W / H:");
    let mut ry = IntInput::new(256, 184, 80, 26, None);
    let mut rw = IntInput::new(342, 184, 100, 26, None);
    let mut rh = IntInput::new(448, 184, 112, 26, None);
    let mut cursor = CheckButton::new(170, 216, 160, 26, "Capture cursor");
    let mut looping = CheckButton::new(340, 216, 160, 26, "Loop media");
    let mut wsize = Input::new(170, 248, 120, 26, "Webcam size (WxH):");
    let mut wfps = IntInput::new(420, 248, 60, 26, "Webcam FPS:");
    let mut color_btn = Button::new(170, 282, 140, 26, "Pick colour…");
    let mut swatch = Frame::new(320, 282, 60, 26, None);
    swatch.set_frame(FrameType::DownBox);
    let mut hint = Frame::new(20, 316, 540, 66, None);
    hint.set_align(Align::Left | Align::Inside | Align::Wrap | Align::Top);
    hint.set_label_size(12);
    let mut ok = Button::new(370, 396, 90, 30, "OK");
    let mut cancel = Button::new(470, 396, 90, 30, "Cancel");
    win.end();

    let screens = screens_px();
    let mut mons: Vec<(String, String)> = screens
        .iter()
        .enumerate()
        .map(|(i, (x, y, w, h))| {
            (
                i.to_string(),
                format!("Screen {}: {}x{} at {},{}", i + 1, w, h, x, y),
            )
        })
        .collect();
    let union = screens.iter().fold(
        None::<(i32, i32, i32, i32)>,
        |acc, &(x, y, w, h)| match acc {
            None => Some((x, y, x + w, y + h)),
            Some((a, b, c, d)) => Some((a.min(x), b.min(y), c.max(x + w), d.max(y + h))),
        },
    );
    if screens.len() > 1
        && let Some((a, b, c, d)) = union
    {
        mons.push((
            "all".into(),
            format!("All screens: {}x{} at {},{}", c - a, d - b, a, b),
        ));
    }
    fill_choice(&mut monitor, &mons);

    let color = Rc::new(Cell::new((40u8, 40u8, 48u8)));
    let detected_items: Rc<RefCell<DevList>> = Rc::new(RefCell::new(vec![]));
    let result = Rc::new(Cell::new(0)); // 1 ok, 2 cancel

    // Prefill
    let (p_sx, p_sy, p_sw, p_sh) = primary_screen_px();
    rx.set_value(&p_sx.to_string());
    ry.set_value(&p_sy.to_string());
    rw.set_value(&p_sw.to_string());
    rh.set_value(&p_sh.to_string());
    cursor.set_checked(true);
    looping.set_checked(true);
    wfps.set_value("0");
    match init {
        Some(s) => {
            name.set_value(&s.name);
            kind.set_value(s.kind.index());
            match &s.kind {
                SourceKind::Display {
                    target: t,
                    x,
                    y,
                    w,
                    h,
                    cursor: c,
                } => {
                    target.set_value(t);
                    rx.set_value(&x.to_string());
                    ry.set_value(&y.to_string());
                    rw.set_value(&w.to_string());
                    rh.set_value(&h.to_string());
                    cursor.set_checked(*c);
                }
                SourceKind::Webcam { device, size, fps } => {
                    target.set_value(device);
                    wsize.set_value(size);
                    wfps.set_value(&fps.to_string());
                }
                SourceKind::Media { path, looping: l } => {
                    target.set_value(path);
                    looping.set_checked(*l);
                }
                SourceKind::Image { path } => target.set_value(path),
                SourceKind::Color { r, g, b } => color.set((*r, *g, *b)),
            }
        }
        None => {
            kind.set_value(0);
            name.set_value("Display Capture");
            target.set_value(&default_display_target());
            if !mons.is_empty() {
                monitor.set_value(0);
            }
        }
    }
    let (r, g, b) = color.get();
    swatch.set_color(Color::from_rgb(r, g, b));

    {
        let (mut rx, mut ry, mut rw, mut rh, mons) =
            (rx.clone(), ry.clone(), rw.clone(), rh.clone(), mons.clone());
        let screens = screens.clone();
        monitor.set_callback(move |c| {
            let i = c.value();
            if i < 0 {
                return;
            }
            let (x, y, w, h) = match mons.get(i as usize).map(|m| m.0.as_str()) {
                Some("all") => match union {
                    Some((a, b, c2, d)) => (a, b, c2 - a, d - b),
                    None => return,
                },
                Some(_) => match screens.get(i as usize) {
                    Some(s) => *s,
                    None => return,
                },
                None => return,
            };
            rx.set_value(&x.to_string());
            ry.set_value(&y.to_string());
            rw.set_value(&w.to_string());
            rh.set_value(&h.to_string());
        });
    }
    {
        let (mut target, items) = (target.clone(), detected_items.clone());
        detected.set_callback(move |c| {
            if let Some((v, _)) = items.borrow().get(c.value().max(0) as usize) {
                target.set_value(v);
            }
        });
    }
    {
        let (kind, mut target) = (kind.clone(), target.clone());
        browse.set_callback(move |_| {
            let mut fc = dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseFile);
            if kind.value() == 3 {
                fc.set_filter("Images\t*.{png,jpg,jpeg,bmp,gif,webp,tif,tiff}");
            } else if kind.value() == 2 {
                fc.set_filter("Media\t*.{mp4,mkv,mov,webm,avi,flv,ts,gif,mp3,m4v}");
            }
            fc.show();
            let f = fc.filename();
            if !f.as_os_str().is_empty() {
                target.set_value(&f.to_string_lossy());
            }
        });
    }
    {
        let (kind, mut detected, items) = (kind.clone(), detected.clone(), detected_items.clone());
        let ff = st.ffmpeg_path.clone();
        detect_btn.set_callback(move |_| {
            draw::set_cursor(fltk::enums::Cursor::Wait);
            app::flush();
            let list = if kind.value() == 1 {
                detect_video_devices(&ff)
            } else {
                vec![]
            };
            draw::set_cursor(fltk::enums::Cursor::Default);
            fill_choice(&mut detected, &list);
            if list.is_empty() {
                dialog::message_default("No devices detected for this source type.");
            } else {
                detected.set_value(0);
            }
            *items.borrow_mut() = list;
        });
    }
    {
        let (color, mut swatch) = (color.clone(), swatch.clone());
        color_btn.set_callback(move |_| {
            if let Some((r, g, b)) = dialog::color_chooser("Source colour", dialog::ColorMode::Byte)
            {
                color.set((r, g, b));
                swatch.set_color(Color::from_rgb(r, g, b));
                swatch.redraw();
            }
        });
    }
    {
        let (res, mut w) = (result.clone(), win.clone());
        ok.set_callback(move |_| {
            res.set(1);
            w.hide();
        });
    }
    {
        let (res, mut w) = (result.clone(), win.clone());
        cancel.set_callback(move |_| {
            res.set(2);
            w.hide();
        });
    }

    let names = ["Display Capture", "Webcam", "Media", "Image", "Color"];
    let update = |k: i32,
                  target: &mut Input,
                  browse: &mut Button,
                  detected: &mut Choice,
                  detect_btn: &mut Button,
                  monitor: &mut Choice,
                  reg: &mut [&mut IntInput; 4],
                  cursor: &mut CheckButton,
                  looping: &mut CheckButton,
                  wsize: &mut Input,
                  wfps: &mut IntInput,
                  color_btn: &mut Button,
                  hint: &mut Frame| {
        let on = |w: &mut dyn WidgetExt, b: bool| if b { w.activate() } else { w.deactivate() };
        on(target, k != 4);
        on(browse, k == 2 || k == 3);
        on(detected, k == 1);
        on(detect_btn, k == 1);
        on(monitor, k == 0);
        for r in reg.iter_mut() {
            on(*r, k == 0);
        }
        on(cursor, k == 0);
        on(looping, k == 2);
        on(wsize, k == 1);
        on(wfps, k == 1);
        on(color_btn, k == 4);
        hint.set_label(match k {
            0 if cfg!(windows) => "Target 'desktop' captures the region below. Use 'title=Window Title' to capture one window.",
            0 if cfg!(target_os = "macos") => "Target is the AVFoundation screen index (usually 1). Region is cropped from it.",
            0 => "Target is the X11 display (e.g. :0.0). Wayland-only sessions need XWayland; pure Wayland is not supported by x11grab.",
            1 => "Device path (/dev/video0), DirectShow name or AVFoundation index. Press Detect to list devices. Size/FPS blank or 0 = driver default.",
            2 => "Any file ffmpeg can decode. Plays in real time; audio of media files is not mixed (use desktop audio).",
            3 => "PNG/JPEG/etc. Transparency is preserved.",
            _ => "Solid colour fill. Combine with opacity for tints.",
        });
    };
    let mut last_k = -1;
    loop {
        win.make_modal(true);
        win.show();
        while win.shown() {
            app::wait();
            let k = kind.value();
            if k != last_k {
                // Swap default target when the user changes type on a fresh source.
                if last_k >= 0 {
                    let t = target.value();
                    let was_default = t.is_empty()
                        || t == default_display_target()
                        || t == default_webcam_target();
                    if was_default {
                        target.set_value(&match k {
                            0 => default_display_target(),
                            1 => default_webcam_target(),
                            _ => String::new(),
                        });
                    }
                    if names.contains(&name.value().as_str()) {
                        name.set_value(names[k.clamp(0, 4) as usize]);
                    }
                }
                let (mut a, mut b, mut c, mut d) = (rx.clone(), ry.clone(), rw.clone(), rh.clone());
                update(
                    k,
                    &mut target,
                    &mut browse,
                    &mut detected,
                    &mut detect_btn,
                    &mut monitor,
                    &mut [&mut a, &mut b, &mut c, &mut d],
                    &mut cursor,
                    &mut looping,
                    &mut wsize,
                    &mut wfps,
                    &mut color_btn,
                    &mut hint,
                );
                last_k = k;
            }
        }
        if result.get() != 1 {
            return None;
        }
        result.set(0);
        let nm = truncate_chars(name.value().trim(), 64);
        let t = target.value().trim().replace('\0', "");
        let pi = |i: &IntInput| i.value().trim().parse::<i64>().ok();
        let res: Result<SourceKind, String> = match kind.value() {
            0 => match (pi(&rx), pi(&ry), pi(&rw), pi(&rh)) {
                (Some(x), Some(y), Some(w), Some(h))
                    if (1..=16384).contains(&w)
                        && (1..=16384).contains(&h)
                        && (-32768..=32767).contains(&x)
                        && (-32768..=32767).contains(&y) =>
                {
                    Ok(SourceKind::Display {
                        target: t,
                        x: x as i32,
                        y: y as i32,
                        w: w as u32,
                        h: h as u32,
                        cursor: cursor.is_checked(),
                    })
                }
                _ => Err("Region must be numeric; W/H between 1 and 16384.".into()),
            },
            1 => {
                let sz = wsize.value().trim().to_string();
                if t.is_empty() {
                    Err("Enter or detect a device.".into())
                } else if !sz.is_empty() && parse_size(&sz).is_none() {
                    Err("Webcam size must look like 1280x720.".into())
                } else {
                    let f = pi(&wfps).unwrap_or(0).clamp(0, MAX_FPS as i64) as u32;
                    Ok(SourceKind::Webcam {
                        device: t,
                        size: sz,
                        fps: f,
                    })
                }
            }
            2 if t.is_empty() => Err("Choose a media file.".into()),
            2 => Ok(SourceKind::Media {
                path: t,
                looping: looping.is_checked(),
            }),
            3 if t.is_empty() => Err("Choose an image file.".into()),
            3 => Ok(SourceKind::Image { path: t }),
            _ => {
                let (r, g, b) = color.get();
                Ok(SourceKind::Color { r, g, b })
            }
        };
        match res {
            Ok(k) => {
                let nm = if nm.is_empty() {
                    k.label().to_string()
                } else {
                    nm
                };
                return Some((nm, k));
            }
            Err(e) => dialog::alert_default(&e),
        }
    }
}

fn dest_line(d: &Destination) -> String {
    let u = d.url.split('?').next().unwrap_or("");
    format!(
        "@.{} {}  [{}]  {}",
        if d.enabled { "●" } else { "○" },
        d.name,
        service_info(&d.service).label,
        truncate_chars(u, 56)
    )
}

/// Manage the multistream destination list. Returns the edited list on OK.
fn destinations_dialog(cur: &[Destination], ff: &FfInfo) -> Option<Vec<Destination>> {
    let list = Rc::new(RefCell::new(cur.to_vec()));
    let mut win = Window::default()
        .with_size(640, 370)
        .center_screen()
        .with_label("Stream Destinations");
    let mut head = Frame::new(
        15,
        8,
        610,
        40,
        "Every enabled destination receives the same encode (multistream), and each one \
reconnects on its own. Double-click a row to edit.",
    );
    head.set_align(Align::Left | Align::Inside | Align::Wrap);
    head.set_label_size(12);
    let mut br = HoldBrowser::new(15, 52, 490, 262, None);
    let mut b_add = Button::new(515, 52, 110, 28, "Add…");
    let mut b_edit = Button::new(515, 86, 110, 28, "Edit…");
    let mut b_tog = Button::new(515, 120, 110, 28, "Enable/Disable");
    let mut b_rm = Button::new(515, 154, 110, 28, "Remove");
    let mut b_up = Button::new(515, 188, 52, 28, "@8>");
    let mut b_dn = Button::new(573, 188, 52, 28, "@2>");
    let mut ok = Button::new(435, 326, 90, 30, "OK");
    let mut cancel = Button::new(535, 326, 90, 30, "Cancel");
    win.end();

    // Buttons only record an action; the loop below performs it, so no
    // RefCell borrow is ever held across a nested modal dialog.
    let act = Rc::new(Cell::new(0u8));
    let result = Rc::new(Cell::new(0u8));
    for (b, code) in [
        (&mut b_add, 1u8),
        (&mut b_edit, 2),
        (&mut b_tog, 3),
        (&mut b_rm, 4),
        (&mut b_up, 5),
        (&mut b_dn, 6),
    ] {
        let a = act.clone();
        b.set_callback(move |_| a.set(code));
    }
    {
        let a = act.clone();
        br.set_callback(move |_| {
            if app::event_clicks() {
                a.set(2);
            }
        });
    }
    for (b, code) in [(&mut ok, 1u8), (&mut cancel, 2)] {
        let (r, mut w) = (result.clone(), win.clone());
        b.set_callback(move |_| {
            r.set(code);
            w.hide();
        });
    }
    let refresh = |br: &mut HoldBrowser, v: &[Destination], sel: usize| {
        br.clear();
        for d in v {
            br.add(&dest_line(d));
        }
        if sel >= 1 && sel <= v.len() {
            br.select(sel as i32);
        }
    };
    refresh(&mut br, &list.borrow(), 1);
    win.make_modal(true);
    win.show();
    while win.shown() {
        app::wait();
        let a = act.replace(0);
        if a == 0 {
            continue;
        }
        let n = list.borrow().len();
        let sel = br.value().max(0) as usize; // 1-based, 0 = none
        let idx = (sel >= 1 && sel <= n).then(|| sel - 1);
        match (a, idx) {
            (1, _) => {
                if n >= MAX_DESTINATIONS {
                    dialog::alert_default(&format!("At most {MAX_DESTINATIONS} destinations."));
                } else if let Some(d) = dest_edit_dialog(None, ff) {
                    list.borrow_mut().push(d);
                    refresh(&mut br, &list.borrow(), n + 1);
                }
            }
            (2, Some(i)) => {
                let cur = list.borrow()[i].clone();
                if let Some(d) = dest_edit_dialog(Some(&cur), ff) {
                    list.borrow_mut()[i] = d;
                    refresh(&mut br, &list.borrow(), sel);
                }
            }
            (3, Some(i)) => {
                {
                    let mut l = list.borrow_mut();
                    l[i].enabled = !l[i].enabled;
                }
                refresh(&mut br, &list.borrow(), sel);
            }
            (4, Some(i)) => {
                let nm = list.borrow()[i].name.clone();
                if dialog::choice2_default(
                    &format!("Remove destination '{nm}'?"),
                    "Cancel",
                    "Remove",
                    "",
                ) == Some(1)
                {
                    list.borrow_mut().remove(i);
                    refresh(&mut br, &list.borrow(), sel.min(n - 1));
                }
            }
            (5, Some(i)) if i > 0 => {
                list.borrow_mut().swap(i, i - 1);
                refresh(&mut br, &list.borrow(), sel - 1);
            }
            (6, Some(i)) if i + 1 < n => {
                list.borrow_mut().swap(i, i + 1);
                refresh(&mut br, &list.borrow(), sel + 1);
            }
            _ => {}
        }
    }
    (result.get() == 1).then(|| list.borrow().clone())
}

/// Add/Edit one destination with service presets.
/// Audio processing dialog. Every change applies live to the running mixer
/// (watch the meters, listen); Cancel restores the previous settings.
fn audio_mixer_dialog(cur: AudioDsp, live: Arc<Mutex<AudioDsp>>) -> Option<AudioDsp> {
    let mut win = Window::default()
        .with_size(560, 540)
        .center_screen()
        .with_label("Audio Mixer");
    let sl = |x: i32, y: i32, w: i32, l: &str, lo: f64, hi: f64, v: f64| {
        let mut s = HorValueSlider::new(x, y, w, 22, None);
        s.set_range(lo, hi);
        s.set_step(1.0, 1);
        s.set_value(v);
        let mut f = Frame::new(x - 170, y, 165, 22, None).with_label(l);
        f.set_align(Align::Right | Align::Inside);
        f.set_label_size(12);
        s
    };
    let hdr = |y: i32, t: &str| {
        let mut f = Frame::new(15, y, 530, 22, None).with_label(t);
        f.set_label_font(Font::HelveticaBold);
        f.set_align(Align::Left | Align::Inside);
    };
    hdr(10, "Balance");
    let target = sl(
        190,
        36,
        340,
        "Target loudness (dBFS):",
        -36.0,
        -6.0,
        cur.target_db as f64,
    );
    let mut note = Frame::new(20, 60, 520, 20, None).with_label(
        "Auto-level drives Mic and Desktop to this level, so neither drowns out the other.",
    );
    note.set_label_size(11);
    note.set_align(Align::Left | Align::Inside);
    hdr(88, "Microphone");
    let m_level = CheckButton::new(20, 112, 250, 22, "Auto-level (normalize)");
    let m_gate = CheckButton::new(20, 138, 160, 22, "Noise gate");
    let gate = sl(
        370,
        138,
        160,
        "open above (dBFS):",
        -80.0,
        -20.0,
        cur.gate_db as f64,
    );
    let m_comp = CheckButton::new(20, 164, 160, 22, "Compressor");
    let comp = sl(
        370,
        164,
        160,
        "threshold (dBFS):",
        -50.0,
        -6.0,
        cur.comp_db as f64,
    );
    let ratio = sl(
        370,
        190,
        160,
        "ratio (:1):",
        1.0,
        20.0,
        cur.comp_ratio as f64,
    );
    let m_trim = sl(
        190,
        216,
        250,
        "Mic trim (dB):",
        -30.0,
        20.0,
        cur.mic_trim_db as f64,
    );
    let m_mute = CheckButton::new(455, 216, 80, 22, "Mute");
    hdr(250, "Desktop / system audio");
    let d_level = CheckButton::new(20, 274, 250, 22, "Auto-level (normalize)");
    let duck = sl(
        190,
        300,
        340,
        "Duck under voice (dB):",
        0.0,
        30.0,
        cur.duck_db as f64,
    );
    let d_trim = sl(
        190,
        326,
        250,
        "Desktop trim (dB):",
        -30.0,
        20.0,
        cur.desk_trim_db as f64,
    );
    let d_mute = CheckButton::new(455, 326, 80, 22, "Mute");
    hdr(360, "Master");
    let limiter = CheckButton::new(
        20,
        384,
        400,
        22,
        "Peak limiter at −1 dBFS (prevents clipping)",
    );
    let mut help = Frame::new(20, 412, 520, 70, None).with_label(
        "Meters: bar = loudness (RMS), white line = peak, tick = peak hold, blue = target.\n\
         GR = gain reduction by compressor/limiter; ±N = auto-level gain.\n\
         Ducking 0 = off. Changes are live; OK saves them to the profile.",
    );
    help.set_label_size(11);
    help.set_align(Align::Left | Align::Inside | Align::Top | Align::Wrap);
    let mut reset = Button::new(20, 495, 120, 30, "Defaults");
    let mut ok = Button::new(350, 495, 90, 30, "OK");
    let mut cancel = Button::new(450, 495, 90, 30, "Cancel");
    win.end();
    m_level.set_checked(cur.mic_level);
    m_gate.set_checked(cur.mic_gate);
    m_comp.set_checked(cur.mic_comp);
    m_mute.set_checked(cur.mic_mute);
    d_level.set_checked(cur.desk_level);
    d_mute.set_checked(cur.desk_mute);
    limiter.set_checked(cur.limiter);

    let read = {
        let (target, gate, comp, ratio, m_trim, duck, d_trim) = (
            target.clone(),
            gate.clone(),
            comp.clone(),
            ratio.clone(),
            m_trim.clone(),
            duck.clone(),
            d_trim.clone(),
        );
        let (m_level, m_gate, m_comp, m_mute, d_level, d_mute, limiter) = (
            m_level.clone(),
            m_gate.clone(),
            m_comp.clone(),
            m_mute.clone(),
            d_level.clone(),
            d_mute.clone(),
            limiter.clone(),
        );
        Rc::new(move || {
            let mut d = AudioDsp {
                target_db: target.value().round() as i32,
                mic_gate: m_gate.is_checked(),
                gate_db: gate.value().round() as i32,
                mic_comp: m_comp.is_checked(),
                comp_db: comp.value().round() as i32,
                comp_ratio: ratio.value().round() as u32,
                mic_level: m_level.is_checked(),
                desk_level: d_level.is_checked(),
                duck_db: duck.value().round() as u32,
                mic_trim_db: m_trim.value().round() as i32,
                desk_trim_db: d_trim.value().round() as i32,
                mic_mute: m_mute.is_checked(),
                desk_mute: d_mute.is_checked(),
                limiter: limiter.is_checked(),
            };
            d.sanitize();
            d
        })
    };
    let push = {
        let (read, live) = (read.clone(), live.clone());
        move || *lock(&live) = read()
    };
    for mut w in [
        target.clone(),
        gate.clone(),
        comp.clone(),
        ratio.clone(),
        m_trim.clone(),
        duck.clone(),
        d_trim.clone(),
    ] {
        let push = push.clone();
        w.set_callback(move |_| push());
    }
    for mut w in [
        m_level.clone(),
        m_gate.clone(),
        m_comp.clone(),
        m_mute.clone(),
        d_level.clone(),
        d_mute.clone(),
        limiter.clone(),
    ] {
        let push = push.clone();
        w.set_callback(move |_| push());
    }
    {
        let d = AudioDsp::default();
        let (mut target, mut gate, mut comp, mut ratio, mut m_trim, mut duck, mut d_trim) = (
            target.clone(),
            gate.clone(),
            comp.clone(),
            ratio.clone(),
            m_trim.clone(),
            duck.clone(),
            d_trim.clone(),
        );
        let (m_level, m_gate, m_comp, m_mute, d_level, d_mute, limiter) = (
            m_level.clone(),
            m_gate.clone(),
            m_comp.clone(),
            m_mute.clone(),
            d_level.clone(),
            d_mute.clone(),
            limiter.clone(),
        );
        let push = push.clone();
        reset.set_callback(move |_| {
            target.set_value(d.target_db as f64);
            gate.set_value(d.gate_db as f64);
            comp.set_value(d.comp_db as f64);
            ratio.set_value(d.comp_ratio as f64);
            m_trim.set_value(d.mic_trim_db as f64);
            duck.set_value(d.duck_db as f64);
            d_trim.set_value(d.desk_trim_db as f64);
            m_level.set_checked(d.mic_level);
            m_gate.set_checked(d.mic_gate);
            m_comp.set_checked(d.mic_comp);
            m_mute.set_checked(d.mic_mute);
            d_level.set_checked(d.desk_level);
            d_mute.set_checked(d.desk_mute);
            limiter.set_checked(d.limiter);
            push();
        });
    }
    let result = Rc::new(Cell::new(0));
    {
        let (res, mut w) = (result.clone(), win.clone());
        ok.set_callback(move |_| {
            res.set(1);
            w.hide();
        });
    }
    {
        let (res, mut w) = (result.clone(), win.clone());
        cancel.set_callback(move |_| {
            res.set(2);
            w.hide();
        });
    }
    modal_run(&mut win);
    (result.get() == 1).then(|| read())
}

fn dest_edit_dialog(init: Option<&Destination>, ff: &FfInfo) -> Option<Destination> {
    let mut win = Window::default()
        .with_size(620, 330)
        .center_screen()
        .with_label(if init.is_some() {
            "Edit Destination"
        } else {
            "Add Destination"
        });
    let mut svc = Choice::new(130, 15, 200, 26, "Service:");
    for x in SERVICES {
        svc.add(
            &menu_escape(x.label),
            Shortcut::None,
            MenuFlag::Normal,
            |_| {},
        );
    }
    let mut name = Input::new(130, 49, 300, 26, "Name:");
    let mut url = Input::new(130, 83, 470, 26, "Server URL:");
    let mut key = SecretInput::new(130, 117, 370, 26, "Stream key:");
    let mut show = Button::new(510, 117, 90, 26, "Show key");
    let en = CheckButton::new(130, 151, 200, 26, "Enabled");
    let mut hint = Frame::new(20, 185, 580, 90, None);
    hint.set_align(Align::Left | Align::Inside | Align::Wrap | Align::Top);
    hint.set_label_size(12);
    let mut ok = Button::new(410, 288, 90, 30, "OK");
    let mut cancel = Button::new(510, 288, 90, 30, "Cancel");
    win.end();

    let d0 = init.cloned().unwrap_or_else(|| Destination {
        name: SERVICES[0].label.into(),
        service: SERVICES[0].id.into(),
        url: SERVICES[0].url.into(),
        key: String::new(),
        enabled: true,
    });
    let si = SERVICES
        .iter()
        .position(|x| x.id == d0.service)
        .unwrap_or(SERVICES.len() - 1);
    svc.set_value(si as i32);
    name.set_value(&d0.name);
    url.set_value(&d0.url);
    key.set_value(&d0.key);
    en.set_checked(d0.enabled);
    {
        let key = key.clone();
        show.set_callback(move |_| {
            // SecretInput can't toggle masking; reveal on demand only.
            let k = key.value();
            dialog::message_default(&if k.is_empty() {
                "(empty)".to_string()
            } else {
                k
            });
        });
    }
    let result = Rc::new(Cell::new(0u8));
    for (b, code) in [(&mut ok, 1u8), (&mut cancel, 2)] {
        let (r, mut w) = (result.clone(), win.clone());
        b.set_callback(move |_| {
            r.set(code);
            w.hide();
        });
    }
    let no_tls = ff.ok && !ff.out_protocols.is_empty() && !ff.out_protocols.contains("rtmps");
    let mut last = -1;
    loop {
        win.make_modal(true);
        win.show();
        while win.shown() {
            app::wait();
            let k = svc.value().clamp(0, SERVICES.len() as i32 - 1);
            if k != last {
                let info = &SERVICES[k as usize];
                if last >= 0 {
                    // Swap presets only where the user hasn't typed their own value.
                    let u = url.value();
                    if u.trim().is_empty()
                        || SERVICES
                            .iter()
                            .any(|x| !x.url.is_empty() && x.url == u.trim())
                    {
                        url.set_value(info.url);
                    }
                    let n = name.value();
                    if n.trim().is_empty() || SERVICES.iter().any(|x| x.label == n.trim()) {
                        name.set_value(info.label);
                    }
                }
                let mut h = info.hint.to_string();
                if info.tls_only && no_tls {
                    h.push_str(
                        "\n⚠ This ffmpeg build lacks RTMPS support — Kick will not work with it.",
                    );
                }
                hint.set_label(&label_escape(&h));
                last = k;
            }
        }
        if result.get() != 1 {
            return None;
        }
        result.set(0);
        let info = &SERVICES[svc.value().clamp(0, SERVICES.len() as i32 - 1) as usize];
        let nm = truncate_chars(name.value().trim(), 48);
        let d = Destination {
            name: if nm.is_empty() { info.label.into() } else { nm },
            service: info.id.into(),
            url: url.value().trim().to_string(),
            key: key.value().trim().to_string(),
            enabled: en.is_checked(),
        };
        match dest_target(&d) {
            Ok(_) => return Some(d),
            Err(e) => dialog::alert_default(&e),
        }
    }
}

fn settings_dialog(cur: &Settings, ff: &FfInfo) -> Option<Settings> {
    let mut win = Window::default()
        .with_size(640, 620)
        .center_screen()
        .with_label("Settings");
    let x = 190;
    let mut y = 14;
    let mut row = |h: i32| {
        let r = y;
        y += h + 8;
        r
    };
    let r = row(26);
    let mut cw = IntInput::new(x, r, 90, 26, "Canvas W x H:");
    let mut chh = IntInput::new(x + 100, r, 90, 26, None);
    let mut fps = IntInput::new(x + 330, r, 60, 26, "FPS:");
    let r = row(26);
    let mut enc = Choice::new(x, r, 200, 26, "Video encoder:");
    let mut preset = Choice::new(x + 330, r, 110, 26, "x264 preset:");
    let r = row(26);
    let mut vk = IntInput::new(x, r, 90, 26, "Video kbps:");
    let mut ak = IntInput::new(x + 190, r, 60, 26, "Audio kbps:");
    let mut ki = IntInput::new(x + 380, r, 50, 26, "Keyint s:");
    let r = row(26);
    let mut recdir = Input::new(x, r, 330, 26, "Recording folder:");
    let mut recbrowse = Button::new(x + 340, r, 90, 26, "Browse…");
    let r = row(26);
    let mut fmt = Choice::new(x, r, 100, 26, "Recording format:");
    let mut fmt_hint = Frame::new(
        x + 110,
        r,
        320,
        26,
        "mkv survives crashes; remux to mp4 later",
    );
    fmt_hint.set_label_size(11);
    fmt_hint.set_align(Align::Left | Align::Inside);
    let r = row(26);
    let mut dest_sum = Frame::new(x, r, 330, 26, "Stream destinations:");
    dest_sum.set_frame(FrameType::DownBox);
    dest_sum.set_align(Align::Left); // caption outside, like the inputs
    let mut dest_txt = Frame::new(x + 4, r, 322, 26, None);
    dest_txt.set_align(Align::Left | Align::Inside | Align::Clip);
    dest_txt.set_label_size(12);
    let mut dest_btn = Button::new(x + 340, r, 90, 26, "Edit…");
    let r = row(26);
    let mut dest_hint = Frame::new(
        x,
        r,
        430,
        26,
        "Kick, Rumble, Twitch, YouTube or custom — all enabled ones go live together.",
    );
    dest_hint.set_label_size(11);
    dest_hint.set_align(Align::Left | Align::Inside);
    let r = row(26);
    let mut api = Choice::new(x, r, 150, 26, "Audio backend:");
    let mut detect_btn = Button::new(x + 160, r, 130, 26, "Detect devices");
    let r = row(26);
    let mut mic = Input::new(x, r, 240, 26, "Mic device:");
    let mut mic_pick = Choice::new(x + 250, r, 180, 26, None);
    let r = row(26);
    let mut desk = Input::new(x, r, 240, 26, "Desktop audio device:");
    let mut desk_pick = Choice::new(x + 250, r, 180, 26, None);
    let r = row(26);
    let mut ffp = Input::new(x, r, 330, 26, "ffmpeg binary:");
    let mut ffbrowse = Button::new(x + 340, r, 90, 26, "Browse…");
    let r = row(26);
    let mut vaapi = Input::new(x, r, 240, 26, "VAAPI device:");
    // v1.3.3: UI theme, applied live when the dialog is saved.
    let mut theme = Choice::new(x + 320, r, 110, 26, "Theme:");
    let r = row(46);
    let mut info = Frame::new(20, r, 600, 46, None);
    info.set_align(Align::Left | Align::Inside | Align::Wrap);
    info.set_label_size(11);
    let enc_list: Vec<String> = ENCODERS
        .iter()
        .filter(|e| {
            !ff.ok || ff.encoders.is_empty() || ff.encoders.contains(**e) || **e == cur.encoder
        })
        .map(|e| e.to_string())
        .collect();
    info.set_label(&label_escape(&format!(
        "{}\nAvailable H.264 encoders in this ffmpeg: {}. Leave a device blank to disable it.",
        if ff.ok {
            ff.version.as_str()
        } else {
            "ffmpeg NOT FOUND — set the binary path above."
        },
        enc_list.join(", ")
    )));
    let mut ok = Button::new(430, 576, 90, 30, "OK");
    let mut cancel = Button::new(530, 576, 90, 30, "Cancel");
    win.end();

    cw.set_value(&cur.canvas_w.to_string());
    chh.set_value(&cur.canvas_h.to_string());
    fps.set_value(&cur.fps.to_string());
    for e in &enc_list {
        enc.add(&menu_escape(e), Shortcut::None, MenuFlag::Normal, |_| {});
    }
    enc.set_value(enc_list.iter().position(|e| *e == cur.encoder).unwrap_or(0) as i32);
    preset.add_choice(&X264_PRESETS.join("|"));
    preset.set_value(
        X264_PRESETS
            .iter()
            .position(|p| *p == cur.x264_preset)
            .unwrap_or(2) as i32,
    );
    vk.set_value(&cur.video_kbps.to_string());
    ak.set_value(&cur.audio_kbps.to_string());
    ki.set_value(&cur.keyint_sec.to_string());
    recdir.set_value(&cur.rec_dir);
    fmt.add_choice(&REC_FORMATS.join("|"));
    theme.add_choice("Dark|Light|System");
    theme.set_value(THEMES.iter().position(|t| *t == cur.theme).unwrap_or(0) as i32);
    fmt.set_value(
        REC_FORMATS
            .iter()
            .position(|p| *p == cur.rec_format)
            .unwrap_or(0) as i32,
    );
    let dests = Rc::new(RefCell::new(cur.destinations.clone()));
    dest_txt.set_label(&label_escape(&dest_summary(&dests.borrow())));
    let apis = audio_apis_here();
    api.add_choice(&apis.join("|"));
    api.set_value(apis.iter().position(|p| *p == cur.audio_api).unwrap_or(0) as i32);
    mic.set_value(&cur.mic_device);
    desk.set_value(&cur.desktop_device);
    ffp.set_value(&cur.ffmpeg_path);
    vaapi.set_value(&cur.vaapi_device);

    let devs: Rc<RefCell<DevList>> = Rc::new(RefCell::new(vec![]));
    {
        let (dests, ff) = (dests.clone(), ff.clone());
        let mut dest_txt = dest_txt.clone();
        dest_btn.set_callback(move |_| {
            let cur = dests.borrow().clone();
            if let Some(v) = destinations_dialog(&cur, &ff) {
                dest_txt.set_label(&label_escape(&dest_summary(&v)));
                *dests.borrow_mut() = v;
            }
        });
    }
    {
        let mut recdir = recdir.clone();
        recbrowse.set_callback(move |_| {
            let mut fc = dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseDir);
            fc.show();
            let f = fc.filename();
            if !f.as_os_str().is_empty() {
                recdir.set_value(&f.to_string_lossy());
            }
        });
    }
    {
        let mut ffp = ffp.clone();
        ffbrowse.set_callback(move |_| {
            let mut fc = dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseFile);
            fc.show();
            let f = fc.filename();
            if !f.as_os_str().is_empty() {
                ffp.set_value(&f.to_string_lossy());
            }
        });
    }
    {
        let (api, ffp, devs) = (api.clone(), ffp.clone(), devs.clone());
        let (mut mp, mut dp) = (mic_pick.clone(), desk_pick.clone());
        detect_btn.set_callback(move |_| {
            let a = audio_apis_here()
                .get(api.value().max(0) as usize)
                .copied()
                .unwrap_or("none");
            draw::set_cursor(fltk::enums::Cursor::Wait);
            app::flush();
            let list = detect_audio_devices(a, &ffp.value());
            draw::set_cursor(fltk::enums::Cursor::Default);
            fill_choice(&mut mp, &list);
            fill_choice(&mut dp, &list);
            if list.is_empty() {
                dialog::message_default("No audio devices detected for this backend.");
            }
            *devs.borrow_mut() = list;
        });
    }
    for (pick, inp) in [(&mut mic_pick, mic.clone()), (&mut desk_pick, desk.clone())] {
        let devs = devs.clone();
        let mut inp = inp;
        pick.set_callback(move |c| {
            if let Some((v, _)) = devs.borrow().get(c.value().max(0) as usize) {
                inp.set_value(v);
            }
        });
    }
    let result = Rc::new(Cell::new(0));
    {
        let (res, mut w) = (result.clone(), win.clone());
        ok.set_callback(move |_| {
            res.set(1);
            w.hide();
        });
    }
    {
        let (res, mut w) = (result.clone(), win.clone());
        cancel.set_callback(move |_| {
            res.set(2);
            w.hide();
        });
    }
    loop {
        modal_run(&mut win);
        if result.get() != 1 {
            return None;
        }
        result.set(0);
        let num = |i: &IntInput| i.value().trim().parse::<u32>().ok();
        let mut s = cur.clone();
        let mut errs = vec![];
        match (num(&cw), num(&chh)) {
            (Some(w), Some(h)) if (160..=MAX_W).contains(&w) && (120..=MAX_H).contains(&h) => {
                s.canvas_w = even(w);
                s.canvas_h = even(h);
            }
            _ => errs.push(format!("Canvas must be 160..{MAX_W} x 120..{MAX_H}.")),
        }
        match num(&fps) {
            Some(f) if (1..=MAX_FPS).contains(&f) => s.fps = f,
            _ => errs.push(format!("FPS must be 1..{MAX_FPS}.")),
        }
        match num(&vk) {
            Some(v) if (100..=200_000).contains(&v) => s.video_kbps = v,
            _ => errs.push("Video kbps must be 100..200000.".into()),
        }
        match num(&ak) {
            Some(v) if (32..=512).contains(&v) => s.audio_kbps = v,
            _ => errs.push("Audio kbps must be 32..512.".into()),
        }
        match num(&ki) {
            Some(v) if (1..=20).contains(&v) => s.keyint_sec = v,
            _ => errs.push("Keyframe interval must be 1..20 s.".into()),
        }
        s.encoder = enc_list
            .get(enc.value().max(0) as usize)
            .cloned()
            .unwrap_or_else(|| "libx264".into());
        s.x264_preset = X264_PRESETS
            .get(preset.value().max(0) as usize)
            .unwrap_or(&"veryfast")
            .to_string();
        s.rec_format = REC_FORMATS
            .get(fmt.value().max(0) as usize)
            .unwrap_or(&"mkv")
            .to_string();
        s.theme = THEMES
            .get(theme.value().max(0) as usize)
            .unwrap_or(&"dark")
            .to_string();
        s.audio_api = apis
            .get(api.value().max(0) as usize)
            .copied()
            .unwrap_or("none")
            .to_string();
        s.rec_dir = recdir.value().trim().to_string();
        s.destinations = dests.borrow().clone();
        s.mic_device = mic.value().trim().to_string();
        s.desktop_device = desk.value().trim().to_string();
        s.ffmpeg_path = ffp.value().trim().to_string();
        s.vaapi_device = vaapi.value().trim().to_string();
        if s.ffmpeg_path.is_empty() {
            errs.push("ffmpeg binary path is required.".into());
        }
        if errs.is_empty() {
            return Some(s);
        }
        dialog::alert_default(&errs.join("\n"));
    }
}

// ----------------------------------------------------------------------------
// Application state & message handling (UI thread only)
// ----------------------------------------------------------------------------
struct DragState {
    id: u64,
    start: (i32, i32),
    orig: (i32, i32, u32, u32),
    resize: bool,
    moved: bool,
}

struct AppState {
    prof: Profile,
    profile_path: PathBuf,
    ui: Ui,
    log: Logger,
    log_seq: u64,
    ff: FfInfo,
    pv: Arc<PreviewShared>,
    ov: Arc<Mutex<Overlay>>,
    comp: Option<Compositor>,
    runtimes: HashMap<String, SourceRuntime>,
    rec: Option<OutputSession>,
    stream: Option<OutputSession>,
    stream_user_on: bool,
    stream_retry: Option<(Instant, u32)>,
    stoppers: Vec<JoinHandle<()>>,
    sel: Option<u64>,
    drag: Option<DragState>,
    dirty: bool,
    last_save: Instant,
    quit: bool,
    auto_record: Option<(Instant, Duration)>, // (start at, duration)
    auto_record_stop: Option<Instant>,
    auto_stream: Option<(Instant, Duration)>,
    auto_stream_stop: Option<Instant>,
    rec_path: Option<PathBuf>,
    /// Running audio engine (live meters); outputs hold their own Arc.
    audio_hub: Option<Arc<AudioHub>>,
    /// Live DSP parameters shared with the mixer thread.
    dsp: Arc<Mutex<AudioDsp>>,
    /// Latest VU snapshot from the mixer, drawn by the meter widget.
    meters: Arc<Mutex<Meters>>,
}

impl AppState {
    fn scene(&self) -> &Scene {
        &self.prof.scenes[self.prof.current]
    }
    fn scene_mut(&mut self) -> &mut Scene {
        let c = self.prof.current;
        &mut self.prof.scenes[c]
    }
    fn sel_index(&self) -> Option<usize> {
        let id = self.sel?;
        self.scene().sources.iter().position(|s| s.id == id)
    }
    fn outputs_active(&self) -> bool {
        self.rec.is_some() || self.stream.is_some() || self.stream_retry.is_some()
    }

    // --- pipeline -----------------------------------------------------------
    fn start_pipeline(&mut self) {
        let s = &self.prof.settings;
        {
            let mut o = lock(&self.ov);
            o.cw = s.canvas_w;
            o.ch = s.canvas_h;
        }
        self.comp = Some(Compositor::start(
            s.canvas_w,
            s.canvas_h,
            s.fps,
            self.pv.clone(),
        ));
        self.apply_scene();
        self.ensure_audio();
    }
    fn stop_pipeline(&mut self) {
        for (_, rt) in self.runtimes.drain() {
            rt.stop();
        }
        if let Some(c) = self.comp.take() {
            c.stop();
        }
    }

    /// Start/stop source processes so exactly the current scene's visible
    /// sources run, then publish the new layer list.
    fn apply_scene(&mut self) {
        let (fps, ff) = (
            self.prof.settings.fps,
            self.prof.settings.ffmpeg_path.clone(),
        );
        let want: Vec<(String, Source)> = self
            .scene()
            .sources
            .iter()
            .filter(|s| s.visible)
            .map(|s| (source_sig(s, fps, &ff), s.clone()))
            .collect();
        let keep: HashSet<&String> = want.iter().map(|(k, _)| k).collect();
        let dead: Vec<String> = self
            .runtimes
            .keys()
            .filter(|k| !keep.contains(k))
            .cloned()
            .collect();
        for k in dead {
            if let Some(rt) = self.runtimes.remove(&k) {
                rt.stop();
            }
        }
        let ctx = RtCtx {
            ffmpeg: ff,
            fps,
            log: self.log.clone(),
        };
        for (k, s) in &want {
            if !self.runtimes.contains_key(k) {
                self.runtimes
                    .insert(k.clone(), SourceRuntime::start(s, &ctx));
            }
        }
        self.publish_layers();
    }

    /// Cheap: re-publish positions/opacity without touching processes.
    fn publish_layers(&mut self) {
        let (fps, ff) = (
            self.prof.settings.fps,
            self.prof.settings.ffmpeg_path.clone(),
        );
        let layers: Vec<Layer> = self
            .scene()
            .sources
            .iter()
            .filter(|s| s.visible)
            .filter_map(|s| {
                let rt = self.runtimes.get(&source_sig(s, fps, &ff))?;
                Some(Layer {
                    x: s.x,
                    y: s.y,
                    w: s.w,
                    h: s.h,
                    opacity: s.opacity,
                    slot: rt.slot.clone(),
                    opaque: rt.opaque.clone(),
                })
            })
            .collect();
        if let Some(c) = &self.comp {
            *lock(&c.sh.layers) = Arc::new(layers);
        }
        self.update_overlay();
    }

    fn update_overlay(&mut self) {
        let sel = self.sel_index().map(|i| {
            let s = &self.scene().sources[i];
            (s.x, s.y, s.w as i32, s.h as i32)
        });
        let sel = match &self.drag {
            Some(d) if d.resize => {
                let s = self.sel_index().map(|i| &self.scene().sources[i]);
                s.map(|s| (s.x, s.y, s.w as i32, s.h as i32)).or(sel)
            }
            _ => sel,
        };
        lock(&self.ov).sel = sel;
        self.ui.preview.redraw();
    }

    // --- widgets -------------------------------------------------------------
    fn refresh_scenes(&mut self) {
        let b = &mut self.ui.scenes;
        b.clear();
        for sc in &self.prof.scenes {
            b.add(&format!("@.{}", sc.name));
        }
        b.select(self.prof.current as i32 + 1);
    }
    fn refresh_sources(&mut self) {
        let srcs = self.scene().sources.clone();
        let sel = self.sel;
        let b = &mut self.ui.sources;
        b.clear();
        let mut line = 0;
        for (i, s) in srcs.iter().enumerate().rev() {
            b.add(&format!(
                "@.{} {}  [{}]",
                if s.visible { "●" } else { "○" },
                s.name,
                s.kind.label()
            ));
            if Some(s.id) == sel {
                line = (srcs.len() - i) as i32;
            }
        }
        if line > 0 {
            b.select(line);
        }
        self.load_props();
    }
    fn load_props(&mut self) {
        let s = self.sel_index().map(|i| self.scene().sources[i].clone());
        let ui = &mut self.ui;
        match s {
            Some(s) => {
                ui.px.set_value(&s.x.to_string());
                ui.py.set_value(&s.y.to_string());
                ui.pw.set_value(&s.w.to_string());
                ui.ph.set_value(&s.h.to_string());
                ui.opac
                    .set_value((s.opacity as f64 * 100.0 / 255.0).round());
            }
            None => {
                for i in [&mut ui.px, &mut ui.py, &mut ui.pw, &mut ui.ph] {
                    i.set_value("");
                }
            }
        }
        self.update_overlay();
    }
    fn browser_to_index(&self, line: i32) -> Option<usize> {
        let n = self.scene().sources.len();
        (line >= 1 && (line as usize) <= n).then(|| n - line as usize)
    }
    /// v1.3.3: apply the profile's theme and recolour the widgets that
    /// carry explicit colours. Runs at startup and after Settings/Load.
    fn apply_ui_theme(&mut self) {
        apply_theme(&self.prof.settings.theme);
        for h in &mut self.ui.headers {
            h.set_label_color(Pal::header());
        }
        self.ui.logview.set_text_color(Pal::log_text());
        self.ui.status.set_color(Pal::status_bg());
        self.update_buttons();
        app::redraw();
    }

    fn update_buttons(&mut self) {
        let rec = self.rec.is_some();
        let live = self.stream.is_some() || self.stream_retry.is_some();
        let b = &mut self.ui.btn_rec;
        b.set_label(if rec {
            "Stop Recording"
        } else {
            "Start Recording"
        });
        b.set_color(if rec {
            Color::from_rgb(200, 50, 50)
        } else {
            Pal::idle_rec() // v1.3.3: tinted idle state
        });
        b.set_label_color(if rec { Color::White } else { Color::Foreground });
        let b = &mut self.ui.btn_stream;
        b.set_label(if live {
            "Stop Streaming"
        } else {
            "Start Streaming"
        });
        b.set_color(if live {
            Color::from_rgb(50, 110, 200)
        } else {
            Pal::idle_live() // v1.3.3: tinted idle state
        });
        b.set_label_color(if live {
            Color::White
        } else {
            Color::Foreground
        });
        self.ui.btn_rec.redraw();
        self.ui.btn_stream.redraw();
    }
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn save(&mut self) {
        match save_profile(&self.profile_path, &self.prof) {
            Ok(()) => {
                self.dirty = false;
                self.last_save = Instant::now();
            }
            Err(e) => self.log.log(format!("profile save failed: {e}")),
        }
    }

    fn next_id(&self) -> u64 {
        self.prof
            .scenes
            .iter()
            .flat_map(|s| s.sources.iter().map(|x| x.id))
            .max()
            .unwrap_or(0)
            + 1
    }

    fn canvas_xy(&self, mx: i32, my: i32) -> Option<(i32, i32)> {
        let s = &self.prof.settings;
        let (ox, oy, pw, ph) = fit_rect(
            s.canvas_w,
            s.canvas_h,
            self.ui.preview.w(),
            self.ui.preview.h(),
        );
        if pw <= 0 || ph <= 0 {
            return None;
        }
        Some((
            ((mx - ox) as f64 * s.canvas_w as f64 / pw as f64).round() as i32,
            ((my - oy) as f64 * s.canvas_h as f64 / ph as f64).round() as i32,
        ))
    }

    // --- outputs -------------------------------------------------------------
    /// Bring the audio engine in line with the settings: restart capture
    /// only if the sources changed; DSP parameters apply live.
    fn ensure_audio(&mut self) {
        *lock(&self.dsp) = self.prof.settings.audio_dsp;
        let src = audio_sources(&self.prof.settings, &self.ff);
        let key = format!("{src:?}");
        if self
            .audio_hub
            .as_ref()
            .map(|h| h.key == key)
            .unwrap_or(false)
        {
            return;
        }
        // Old hub keeps running for any output still holding it.
        self.audio_hub = None;
        *lock(&self.meters) = Meters::default();
        if !src.is_empty() {
            self.audio_hub = Some(AudioHub::start(
                key,
                src,
                self.dsp.clone(),
                self.meters.clone(),
                &self.log,
            ));
        }
    }

    /// Audio hub for a new output: reuses the running one when the audio
    /// settings are unchanged (record + stream share one capture).
    fn audio_hub(&mut self) -> Option<Arc<AudioHub>> {
        self.ensure_audio();
        self.audio_hub.clone()
    }

    fn start_record(&mut self) -> Result<(), String> {
        let comp = self.comp.as_ref().ok_or("pipeline not running")?.sh.clone();
        let s = self.prof.settings.clone();
        let dir = PathBuf::from(&s.rec_dir);
        fs::create_dir_all(&dir).map_err(|e| format!("recording folder {}: {e}", dir.display()))?;
        let path = unique_path(&dir, &format!("litecast_{}", utc_stamp()), &s.rec_format);
        let p = path.to_string_lossy().into_owned();
        let hub = self.audio_hub();
        let sess = OutputSession::start(
            OutKind::Record,
            &s,
            &self.ff,
            &comp,
            &self.log,
            &[("file".to_string(), p.clone())],
            p.clone(),
            hub,
        )?;
        self.rec = Some(sess);
        self.rec_path = Some(path);
        Ok(())
    }
    fn stop_record(&mut self) {
        if let Some(r) = self.rec.take() {
            if let Some(h) = r.begin_stop(self.log.clone()) {
                self.stoppers.push(h);
            }
            if let Some(p) = self.rec_path.take() {
                self.log.log(format!("[rec] finalising {}", p.display()));
            }
        }
    }
    fn start_stream(&mut self) -> Result<(), String> {
        let comp = self.comp.as_ref().ok_or("pipeline not running")?.sh.clone();
        let s = self.prof.settings.clone();
        let (targets, warns) = stream_preflight(&s, &self.ff)?;
        self.log.set_secrets(secrets_of(&s));
        for w in &warns {
            self.log.log(format!("[stream] warning: {w}"));
        }
        let names: Vec<&str> = targets.iter().map(|t| t.0.as_str()).collect();
        let label = format!("{} (keys hidden)", names.join(", "));
        let hub = self.audio_hub();
        self.stream = Some(OutputSession::start(
            OutKind::Stream,
            &s,
            &self.ff,
            &comp,
            &self.log,
            &targets,
            label,
            hub,
        )?);
        Ok(())
    }
    fn stop_stream(&mut self) {
        self.stream_retry = None;
        self.stream_user_on = false;
        if let Some(r) = self.stream.take()
            && let Some(h) = r.begin_stop(self.log.clone())
        {
            self.stoppers.push(h);
        }
    }

    fn screenshot(&mut self) {
        let Some(c) = &self.comp else { return };
        let Some(frame) = lock(&c.sh.canvas).clone() else {
            self.log.log("screenshot: no frame yet");
            return;
        };
        let s = &self.prof.settings;
        let dir = PathBuf::from(&s.rec_dir);
        let _ = fs::create_dir_all(&dir);
        let path = unique_path(&dir, &format!("litecast_shot_{}", utc_stamp()), "png");
        let args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-n".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            PIX.into(),
            "-video_size".into(),
            format!("{}x{}", c.sh.cw, c.sh.ch),
            "-i".into(),
            "pipe:0".into(),
            "-frames:v".into(),
            "1".into(),
            local_file_arg(&path.to_string_lossy()), // v1.3.1 FIX: never a URL
        ];
        let (ff, log) = (s.ffmpeg_path.clone(), self.log.clone());
        let h = thread::spawn(move || {
            match run_tool(&ff, &args, Some(frame), 1024, Duration::from_secs(20), None) {
                Ok((_, _, Some(0))) => log.log(format!("screenshot saved: {}", path.display())),
                Ok((_, e, _)) => log.log(format!(
                    "screenshot failed: {}",
                    truncate_chars(e.trim(), 200)
                )),
                Err(e) => log.log(format!("screenshot failed: {e}")),
            }
        });
        self.stoppers.push(h);
    }

    // --- periodic --------------------------------------------------------------
    fn tick(&mut self) {
        // Request preview size matching the widget.
        let s = &self.prof.settings;
        let (_, _, pw, ph) = fit_rect(
            s.canvas_w,
            s.canvas_h,
            self.ui.preview.w(),
            self.ui.preview.h(),
        );
        self.pv
            .req
            .store(((pw.max(0) as u64) << 32) | ph.max(0) as u64, Relaxed);

        // Output health.
        if self.rec.as_ref().map(|r| !r.alive()).unwrap_or(false) {
            let err = self
                .rec
                .as_ref()
                .map(|r| lock(&r.last_err).clone())
                .unwrap_or_default();
            self.log
                .log(format!("[rec] encoder exited unexpectedly: {err}"));
            if self
                .rec
                .as_ref()
                .map(|r| r.started.elapsed() < Duration::from_secs(5))
                .unwrap_or(false)
                && !self.prof.settings.mic_device.is_empty()
            {
                self.log.log(
                    "[rec] hint: check the audio devices in Settings (or set backend to 'none').",
                );
            }
            self.stop_record();
            self.update_buttons();
        }
        if self.stream.as_ref().map(|r| !r.alive()).unwrap_or(false) {
            let ok_for = self
                .stream
                .as_ref()
                .map(|r| r.started.elapsed())
                .unwrap_or_default();
            let err = self
                .stream
                .as_ref()
                .map(|r| lock(&r.last_err).clone())
                .unwrap_or_default();
            self.log.log(format!("[stream] disconnected: {err}"));
            if let Some(r) = self.stream.take()
                && let Some(h) = r.begin_stop(self.log.clone())
            {
                self.stoppers.push(h);
            }
            let attempts = if ok_for > Duration::from_secs(60) {
                0
            } else {
                self.stream_retry.map(|r| r.1).unwrap_or(0)
            };
            if self.stream_user_on && attempts < STREAM_MAX_RETRIES {
                let wait = Duration::from_secs(2u64 << attempts.min(5));
                self.log.log(format!(
                    "[stream] reconnecting in {}s (attempt {}/{STREAM_MAX_RETRIES})",
                    wait.as_secs(),
                    attempts + 1
                ));
                self.stream_retry = Some((Instant::now() + wait, attempts + 1));
            } else {
                if self.stream_user_on {
                    self.log.log("[stream] giving up after repeated failures");
                }
                self.stream_user_on = false;
                self.stream_retry = None;
            }
            self.update_buttons();
        }
        if let Some((at, n)) = self.stream_retry
            && Instant::now() >= at
        {
            match self.start_stream() {
                Ok(()) => {
                    self.stream_retry =
                        Some((Instant::now() + Duration::from_secs(3600 * 24 * 365), n))
                }
                Err(e) => {
                    self.log.log(format!("[stream] reconnect failed: {e}"));
                    self.stream_retry = None;
                    self.stream_user_on = false;
                }
            }
            self.update_buttons();
        }
        // While connected, the retry slot only carries the attempt counter.
        if self.stream.is_some()
            && let Some((_, n)) = self.stream_retry
        {
            if self
                .stream
                .as_ref()
                .map(|s| s.started.elapsed() > Duration::from_secs(60))
                .unwrap_or(false)
            {
                self.stream_retry = None;
            } else {
                self.stream_retry =
                    Some((Instant::now() + Duration::from_secs(3600 * 24 * 365), n));
            }
        }

        // Headless/scheduled recording (--record N).
        if let Some((start_at, dur)) = self.auto_record
            && Instant::now() >= start_at
            && self.rec.is_none()
            && self.auto_record_stop.is_none()
        {
            match self.start_record() {
                Ok(()) => self.auto_record_stop = Some(Instant::now() + dur),
                Err(e) => {
                    self.log.log(format!("auto-record failed: {e}"));
                    eprintln!("auto-record failed: {e}");
                    self.auto_record = None;
                    self.quit = self.auto_stream.is_none();
                }
            }
            self.update_buttons();
        }
        if let Some(t) = self.auto_record_stop
            && (Instant::now() >= t || self.rec.is_none())
        {
            self.stop_record();
            self.auto_record = None;
            self.auto_record_stop = None;
            self.quit = self.auto_stream.is_none();
        }

        // Headless/scheduled streaming (--stream N). Time-based only: drops are
        // handled by per-destination fifo recovery and the session retry logic.
        if let Some((start_at, dur)) = self.auto_stream
            && Instant::now() >= start_at
            && self.stream.is_none()
            && self.auto_stream_stop.is_none()
        {
            match self.start_stream() {
                Ok(()) => {
                    self.stream_user_on = true;
                    self.auto_stream_stop = Some(Instant::now() + dur);
                }
                Err(e) => {
                    self.log.log(format!("auto-stream failed: {e}"));
                    eprintln!("auto-stream failed: {e}");
                    self.auto_stream = None;
                    self.quit = self.auto_record.is_none();
                }
            }
            self.update_buttons();
        }
        if let Some(t) = self.auto_stream_stop
            && Instant::now() >= t
        {
            self.stop_stream();
            self.auto_stream = None;
            self.auto_stream_stop = None;
            self.quit = self.auto_record.is_none();
            self.update_buttons();
        }

        // Reap finished background stoppers.
        let mut i = 0;
        while i < self.stoppers.len() {
            if self.stoppers[i].is_finished() {
                let _ = self.stoppers.swap_remove(i).join();
            } else {
                i += 1;
            }
        }

        // Log view.
        let (seq, lines) = self.log.since(self.log_seq);
        if !lines.is_empty() {
            self.log_seq = seq;
            for l in lines {
                self.ui.logbuf.append(&l);
                self.ui.logbuf.append("\n");
            }
            let len = self.ui.logbuf.length();
            if len > LOG_VIEW_MAX_BYTES {
                let cut = len - LOG_VIEW_MAX_BYTES / 2;
                let cut = self.ui.logbuf.line_end(cut) + 1;
                self.ui.logbuf.remove(0, cut);
            }
            let len = self.ui.logbuf.length();
            self.ui.logview.set_insert_position(len);
            self.ui.logview.show_insert_position();
        }

        // Status bar.
        let mut parts: Vec<String> = vec![];
        if let Some(r) = &self.rec {
            parts.push(format!(
                "● REC {}{}",
                fmt_dur(r.started.elapsed()),
                r.drop_note()
            ));
        }
        if let Some(r) = &self.stream {
            parts.push(format!(
                "● LIVE {} → {}{}",
                fmt_dur(r.started.elapsed()),
                r.health_summary(),
                r.drop_note()
            ));
        } else if let Some((at, n)) = self.stream_retry {
            parts.push(format!(
                "LIVE reconnecting in {}s ({n}/{STREAM_MAX_RETRIES})",
                at.saturating_duration_since(Instant::now()).as_secs()
            ));
        }
        if let Some(c) = &self.comp {
            parts.push(format!(
                "{}x{} @ {:.1} fps",
                c.sh.cw,
                c.sh.ch,
                c.sh.fps_milli.load(Relaxed) as f64 / 1000.0
            ));
            parts.push(format!(
                "compose {:.1} ms",
                c.sh.compose_us.load(Relaxed) as f64 / 1000.0
            ));
            parts.push(format!("lagged {}", c.sh.late.load(Relaxed)));
        }
        parts.push(format!("scene: {}", self.scene().name));
        if !self.ff.ok {
            parts.push("ffmpeg NOT FOUND — see Settings".into());
        }
        self.ui
            .status
            .set_label(&label_escape(&format!("  {}", parts.join("   |   "))));

        // Selected source status.
        let st = self.sel_index().map(|i| {
            let s = &self.scene().sources[i];
            let rt = self.runtimes.get(&source_sig(
                s,
                self.prof.settings.fps,
                &self.prof.settings.ffmpeg_path,
            ));
            match rt {
                Some(rt) => format!("{}: {}", s.name, lock(&rt.status)),
                None => format!("{}: hidden", s.name),
            }
        });
        self.ui
            .src_status
            .set_label(&label_escape(&truncate_chars(&st.unwrap_or_default(), 160)));

        if self.dirty && self.last_save.elapsed() > Duration::from_secs(15) {
            self.save();
        }
    }

    // --- dispatcher ----------------------------------------------------------
    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Tick => self.tick(),
            Msg::Quit => {
                if self.outputs_active() {
                    let c = dialog::choice2_default(
                        "Recording or streaming is active.\nStop and quit?",
                        "Cancel",
                        "Stop && Quit",
                        "",
                    );
                    if c != Some(1) {
                        return;
                    }
                }
                self.quit = true;
            }
            Msg::SceneSelect => {
                let v = self.ui.scenes.value();
                if v >= 1
                    && (v as usize) <= self.prof.scenes.len()
                    && v as usize - 1 != self.prof.current
                {
                    self.prof.current = v as usize - 1;
                    self.sel = None;
                    self.apply_scene();
                    self.refresh_sources();
                    self.mark_dirty();
                }
                self.refresh_scenes();
            }
            Msg::SceneAdd => {
                if self.prof.scenes.len() >= MAX_SCENES {
                    return;
                }
                let def = format!("Scene {}", self.prof.scenes.len() + 1);
                if let Some(n) = dialog::input_default("Scene name:", &def) {
                    let n = truncate_chars(n.trim(), 64);
                    self.prof.scenes.push(Scene {
                        name: if n.is_empty() { def } else { n },
                        sources: vec![],
                    });
                    self.prof.current = self.prof.scenes.len() - 1;
                    self.sel = None;
                    self.apply_scene();
                    self.refresh_scenes();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SceneRemove => {
                if self.prof.scenes.len() <= 1 {
                    dialog::alert_default("At least one scene is required.");
                    return;
                }
                let nm = self.scene().name.clone();
                if dialog::choice2_default(&format!("Remove scene '{nm}'?"), "Cancel", "Remove", "")
                    == Some(1)
                {
                    let c = self.prof.current;
                    self.prof.scenes.remove(c);
                    self.prof.current = c.min(self.prof.scenes.len() - 1);
                    self.sel = None;
                    self.apply_scene();
                    self.refresh_scenes();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SceneRename => {
                let cur = self.scene().name.clone();
                if let Some(n) = dialog::input_default("Scene name:", &cur) {
                    let n = truncate_chars(n.trim(), 64);
                    if !n.is_empty() {
                        self.scene_mut().name = n;
                        self.refresh_scenes();
                        self.mark_dirty();
                    }
                }
            }
            Msg::SceneUp | Msg::SceneDown => {
                let c = self.prof.current;
                let to = if m == Msg::SceneUp {
                    c.checked_sub(1)
                } else {
                    Some(c + 1).filter(|&t| t < self.prof.scenes.len())
                };
                if let Some(t) = to {
                    self.prof.scenes.swap(c, t);
                    self.prof.current = t;
                    self.refresh_scenes();
                    self.mark_dirty();
                }
            }
            Msg::SourceSelect => {
                let line = self.ui.sources.value();
                self.sel = self
                    .browser_to_index(line)
                    .map(|i| self.scene().sources[i].id);
                self.load_props();
            }
            Msg::SourceAdd => {
                if self.scene().sources.len() >= MAX_SOURCES {
                    dialog::alert_default("Source limit reached for this scene.");
                    return;
                }
                let st = self.prof.settings.clone();
                if let Some((name, kind)) = source_dialog(None, &st) {
                    let (cw, ch) = (st.canvas_w, st.canvas_h);
                    let (x, y, w, h) = match &kind {
                        SourceKind::Display { w, h, .. } => fit_rect(*w, *h, cw as i32, ch as i32),
                        SourceKind::Webcam { size, .. } => {
                            let (sw, sh) = parse_size(size).unwrap_or((16, 9));
                            let tw = (cw as i32 / 4).max(2);
                            let th = ((tw as f64 * sh as f64 / sw as f64).round() as i32).max(2);
                            (cw as i32 - tw - 20, ch as i32 - th - 20, tw, th)
                        }
                        _ => (0, 0, cw as i32, ch as i32),
                    };
                    let src = Source {
                        id: self.next_id(),
                        name,
                        kind,
                        visible: true,
                        x,
                        y,
                        w: (w.max(2) as u32).min(MAX_W),
                        h: (h.max(2) as u32).min(MAX_H),
                        opacity: 255,
                    };
                    self.sel = Some(src.id);
                    self.scene_mut().sources.push(src);
                    self.apply_scene();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SourceEdit => {
                let Some(i) = self.sel_index() else { return };
                let cur = self.scene().sources[i].clone();
                let st = self.prof.settings.clone();
                if let Some((name, kind)) = source_dialog(Some(&cur), &st) {
                    if let Some(s) = self.scene_mut().sources.iter_mut().find(|s| s.id == cur.id) {
                        s.name = name;
                        s.kind = kind;
                    }
                    self.apply_scene();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SourceRemove => {
                let Some(i) = self.sel_index() else { return };
                let nm = self.scene().sources[i].name.clone();
                if dialog::choice2_default(
                    &format!("Remove source '{nm}'?"),
                    "Cancel",
                    "Remove",
                    "",
                ) == Some(1)
                {
                    if let Some(i) = self.sel_index() {
                        self.scene_mut().sources.remove(i);
                    }
                    self.sel = None;
                    self.apply_scene();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SourceToggle => {
                let Some(i) = self.sel_index() else { return };
                let v = &mut self.scene_mut().sources[i].visible;
                *v = !*v;
                self.apply_scene();
                self.refresh_sources();
                self.mark_dirty();
            }
            Msg::SourceUp | Msg::SourceDown => {
                let Some(i) = self.sel_index() else { return };
                let n = self.scene().sources.len();
                // "Up" in the list = higher z = larger index.
                let to = if m == Msg::SourceUp {
                    Some(i + 1).filter(|&t| t < n)
                } else {
                    i.checked_sub(1)
                };
                if let Some(t) = to {
                    self.scene_mut().sources.swap(i, t);
                    self.publish_layers();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::PropsApply => {
                let Some(i) = self.sel_index() else { return };
                let p = |w: &IntInput| w.value().trim().parse::<i64>().ok();
                let (x, y, w, h) = (
                    p(&self.ui.px),
                    p(&self.ui.py),
                    p(&self.ui.pw),
                    p(&self.ui.ph),
                );
                let s = &mut self.scene_mut().sources[i];
                let old = (s.w, s.h);
                if let Some(x) = x {
                    s.x = x.clamp(-(MAX_W as i64), MAX_W as i64) as i32;
                }
                if let Some(y) = y {
                    s.y = y.clamp(-(MAX_H as i64), MAX_H as i64) as i32;
                }
                if let Some(w) = w {
                    s.w = w.clamp(MIN_DIM as i64, MAX_W as i64) as u32;
                }
                if let Some(h) = h {
                    s.h = h.clamp(MIN_DIM as i64, MAX_H as i64) as u32;
                }
                let resized = old != (s.w, s.h);
                if resized {
                    self.apply_scene();
                } else {
                    self.publish_layers();
                }
                self.load_props();
                self.mark_dirty();
            }
            Msg::PropsOpacity => {
                let Some(i) = self.sel_index() else { return };
                let o = (self.ui.opac.value().clamp(0.0, 100.0) * 255.0 / 100.0).round() as u8;
                self.scene_mut().sources[i].opacity = o;
                self.publish_layers();
                self.mark_dirty();
            }
            Msg::PropsFill | Msg::PropsCenter | Msg::PropsNative => {
                let Some(i) = self.sel_index() else { return };
                let (cw, ch) = (self.prof.settings.canvas_w, self.prof.settings.canvas_h);
                let s = &mut self.scene_mut().sources[i];
                let old = (s.w, s.h);
                match m {
                    Msg::PropsFill => {
                        let (aw, ah) = match &s.kind {
                            SourceKind::Display { w, h, .. } => (*w, *h),
                            _ => (s.w, s.h),
                        };
                        let (x, y, w, h) = fit_rect(aw, ah, cw as i32, ch as i32);
                        (s.x, s.y, s.w, s.h) = (x, y, w.max(2) as u32, h.max(2) as u32);
                    }
                    Msg::PropsCenter => {
                        s.x = (cw as i32 - s.w as i32) / 2;
                        s.y = (ch as i32 - s.h as i32) / 2;
                    }
                    _ => {
                        if let SourceKind::Display { w, h, .. } = &s.kind {
                            (s.w, s.h) = ((*w).clamp(MIN_DIM, MAX_W), (*h).clamp(MIN_DIM, MAX_H));
                        } else if let SourceKind::Webcam { size, .. } = &s.kind
                            && let Some((w, h)) = parse_size(size)
                        {
                            (s.w, s.h) = (w.max(MIN_DIM), h.max(MIN_DIM));
                        }
                    }
                }
                let resized = old != (s.w, s.h);
                if resized {
                    self.apply_scene();
                } else {
                    self.publish_layers();
                }
                self.load_props();
                self.mark_dirty();
            }
            Msg::ToggleRecord => {
                if self.rec.is_some() {
                    self.stop_record();
                } else if let Err(e) = self.start_record() {
                    self.log.log(format!("[rec] {e}"));
                    dialog::alert_default(&format!("Could not start recording:\n{e}"));
                }
                self.update_buttons();
            }
            Msg::ToggleStream => {
                if self.stream.is_some() || self.stream_retry.is_some() {
                    self.stop_stream();
                } else {
                    match self.start_stream() {
                        Ok(()) => self.stream_user_on = true,
                        Err(e) => {
                            self.log.log(format!("[stream] {e}"));
                            dialog::alert_default(&format!("Could not start streaming:\n{e}"));
                        }
                    }
                }
                self.update_buttons();
            }
            Msg::Screenshot => self.screenshot(),
            Msg::Settings => {
                let cur = self.prof.settings.clone();
                let Some(ns) = settings_dialog(&cur, &self.ff) else {
                    return;
                };
                let needs_restart = ns.canvas_w != cur.canvas_w
                    || ns.canvas_h != cur.canvas_h
                    || ns.fps != cur.fps
                    || ns.ffmpeg_path != cur.ffmpeg_path;
                if needs_restart && self.outputs_active() {
                    dialog::alert_default(
                        "Stop recording/streaming before changing canvas size, FPS or the ffmpeg path.",
                    );
                    return;
                }
                let theme_changed = ns.theme != cur.theme;
                self.prof.settings = ns;
                self.log.set_secrets(secrets_of(&self.prof.settings));
                if theme_changed {
                    self.apply_ui_theme(); // v1.3.3: live, no restart
                }
                if cur.ffmpeg_path != self.prof.settings.ffmpeg_path {
                    self.ff = detect_ffmpeg(&self.prof.settings.ffmpeg_path);
                    self.log_ff();
                }
                if needs_restart {
                    self.stop_pipeline();
                    self.start_pipeline();
                } else {
                    self.ensure_audio();
                }
                if self.ff.ok
                    && !self.ff.encoders.is_empty()
                    && !self.ff.encoders.contains(&self.prof.settings.encoder)
                {
                    self.log.log(format!(
                        "warning: encoder {} not available in this ffmpeg build",
                        self.prof.settings.encoder
                    ));
                }
                self.save();
                if cur.video_kbps != self.prof.settings.video_kbps
                    || cur.keyint_sec != self.prof.settings.keyint_sec
                    || cur.destinations != self.prof.settings.destinations
                {
                    self.show_stream_warnings();
                }
            }
            Msg::Destinations => {
                let cur = self.prof.settings.destinations.clone();
                let Some(v) = destinations_dialog(&cur, &self.ff) else {
                    return;
                };
                self.prof.settings.destinations = v;
                self.log.set_secrets(secrets_of(&self.prof.settings));
                if self.stream.is_some() {
                    self.log.log(
                        "[stream] destination changes apply the next time you start streaming",
                    );
                }
                self.save();
                self.show_stream_warnings();
            }
            Msg::AudioMixer => {
                let cur = self.prof.settings.audio_dsp;
                match audio_mixer_dialog(cur, self.dsp.clone()) {
                    Some(n) => {
                        self.prof.settings.audio_dsp = n;
                        *lock(&self.dsp) = n;
                        self.save();
                    }
                    None => *lock(&self.dsp) = cur, // cancel: undo live edits
                }
            }
            Msg::SaveProfile => {
                self.save();
                self.log
                    .log(format!("profile saved: {}", self.profile_path.display()));
            }
            Msg::SaveProfileAs => {
                let mut fc =
                    dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseSaveFile);
                fc.set_filter("LiteCast profile\t*.json");
                fc.set_option(dialog::NativeFileChooserOptions::SaveAsConfirm);
                fc.show();
                let mut f = fc.filename();
                if f.as_os_str().is_empty() {
                    return;
                }
                if f.extension().is_none() {
                    f.set_extension("json");
                }
                match save_profile(&f, &self.prof) {
                    Ok(()) => {
                        self.profile_path = f;
                        self.dirty = false;
                        self.log
                            .log(format!("profile saved: {}", self.profile_path.display()));
                    }
                    Err(e) => dialog::alert_default(&format!("Save failed: {e}")),
                }
            }
            Msg::LoadProfile => {
                if self.outputs_active() {
                    dialog::alert_default("Stop recording/streaming before loading a profile.");
                    return;
                }
                let mut fc =
                    dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseFile);
                fc.set_filter("LiteCast profile\t*.json");
                fc.show();
                let f = fc.filename();
                if f.as_os_str().is_empty() {
                    return;
                }
                match load_profile(&f) {
                    Ok(p) => {
                        if self.dirty {
                            self.save();
                        }
                        let mut p = p;
                        // v1.3.1 FIX (security): ffmpeg_path is an executable
                        // LiteCast runs. Loading a profile from someone else must
                        // not silently run an arbitrary program: ask first, and
                        // keep the current ffmpeg if the user declines.
                        if p.settings.ffmpeg_path != self.prof.settings.ffmpeg_path {
                            let q = format!(
                                "This profile sets the ffmpeg program to:\n\n{}\n\n\
                                 LiteCast will run this file. Only allow it if you trust \
                                 where the profile came from.",
                                truncate_chars(&p.settings.ffmpeg_path, 300)
                            );
                            if dialog::choice2_default(
                                &label_escape(&q),
                                "Keep current ffmpeg",
                                "Allow",
                                "",
                            ) != Some(1)
                            {
                                p.settings.ffmpeg_path = self.prof.settings.ffmpeg_path.clone();
                                self.log
                                    .log("profile ffmpeg path not trusted; kept the current one");
                            }
                        }
                        self.stop_pipeline();
                        let ff_changed = p.settings.ffmpeg_path != self.prof.settings.ffmpeg_path;
                        self.prof = p;
                        self.profile_path = f;
                        self.sel = None;
                        self.log.set_secrets(secrets_of(&self.prof.settings));
                        if ff_changed {
                            self.ff = detect_ffmpeg(&self.prof.settings.ffmpeg_path);
                            self.log_ff();
                        }
                        self.start_pipeline();
                        self.apply_ui_theme(); // v1.3.3
                        self.refresh_scenes();
                        self.refresh_sources();
                        self.log
                            .log(format!("profile loaded: {}", self.profile_path.display()));
                    }
                    Err(e) => dialog::alert_default(&format!("Load failed: {e}")),
                }
            }
            Msg::About => dialog::message_default(&format!(
                "{APP_NAME} v{APP_VERSION}\nLightweight OBS-style recorder & streamer\nRust + FLTK, encoding via ffmpeg.\n\n{}\nProfile: {}",
                if self.ff.ok {
                    self.ff.version.as_str()
                } else {
                    "ffmpeg not found"
                },
                self.profile_path.display()
            )),
            Msg::PreviewPush(mx, my, shift) => {
                let Some((cx, cy)) = self.canvas_xy(mx, my) else {
                    return;
                };
                let hit = self
                    .scene()
                    .sources
                    .iter()
                    .rev()
                    .find(|s| {
                        s.visible
                            && cx >= s.x
                            && cy >= s.y
                            && (cx as i64) < s.x as i64 + s.w as i64
                            && (cy as i64) < s.y as i64 + s.h as i64
                    })
                    .map(|s| (s.id, s.x, s.y, s.w, s.h));
                match hit {
                    Some((id, x, y, w, h)) => {
                        self.sel = Some(id);
                        // Grabbing the bottom-right handle also resizes.
                        let near_corner =
                            (cx - (x + w as i32)).abs() < 24 && (cy - (y + h as i32)).abs() < 24;
                        self.drag = Some(DragState {
                            id,
                            start: (cx, cy),
                            orig: (x, y, w, h),
                            resize: shift || near_corner,
                            moved: false,
                        });
                    }
                    None => {
                        self.sel = None;
                        self.drag = None;
                    }
                }
                self.refresh_sources();
            }
            Msg::PreviewDrag(mx, my) => {
                let Some((cx, cy)) = self.canvas_xy(mx, my) else {
                    return;
                };
                let Some(d) = &mut self.drag else { return };
                let (dx, dy) = (cx - d.start.0, cy - d.start.1);
                d.moved = true;
                let (id, orig, resize) = (d.id, d.orig, d.resize);
                let Some(s) = self.scene_mut().sources.iter_mut().find(|s| s.id == id) else {
                    return;
                };
                if resize {
                    s.w = (orig.2 as i64 + dx as i64).clamp(MIN_DIM as i64, MAX_W as i64) as u32;
                    s.h = (orig.3 as i64 + dy as i64).clamp(MIN_DIM as i64, MAX_H as i64) as u32;
                    // Outline only while dragging; capture restarts on release.
                    let (x, y, w, h) = (s.x, s.y, s.w as i32, s.h as i32);
                    lock(&self.ov).sel = Some((x, y, w, h));
                    self.ui.preview.redraw();
                } else {
                    s.x = (orig.0 + dx).clamp(-(MAX_W as i32), MAX_W as i32);
                    s.y = (orig.1 + dy).clamp(-(MAX_H as i32), MAX_H as i32);
                    // Move keeps the source signature: no capture restart.
                    self.publish_layers();
                }
            }
            Msg::PreviewRelease => {
                if let Some(d) = self.drag.take()
                    && d.moved
                {
                    if d.resize {
                        // Commit the new size: apply_scene restarts only this source.
                        self.apply_scene();
                    } else {
                        self.publish_layers();
                    }
                    self.load_props();
                    self.mark_dirty();
                }
            }
        }
    }

    /// Non-blocking heads-up about service limits (e.g. Kick bitrate/keyframes).
    fn show_stream_warnings(&self) {
        if !self.prof.settings.destinations.iter().any(|d| d.enabled) {
            return;
        }
        match stream_preflight(&self.prof.settings, &self.ff) {
            Ok((_, w)) => {
                let w: Vec<String> = w
                    .into_iter()
                    .filter(|x| !x.starts_with("no audio"))
                    .collect();
                if !w.is_empty() {
                    dialog::message_default(&format!("Streaming notes:\n\n• {}", w.join("\n• ")));
                }
            }
            Err(e) => dialog::alert_default(&e),
        }
    }

    fn log_ff(&self) {
        if self.ff.ok {
            self.log.log(format!(
                "{} (fps_mode: {})",
                self.ff.version,
                self.ff.has_fps_mode()
            ));
        } else {
            self.log.log(format!(
                "ffmpeg not found at '{}' — set it in Settings",
                self.prof.settings.ffmpeg_path
            ));
        }
    }

    fn shutdown(&mut self) {
        self.ui.win.set_label(&format!("{APP_NAME} — finalising…"));
        app::flush();
        self.stop_record();
        self.stop_stream();
        for h in self.stoppers.drain(..) {
            let _ = h.join();
        }
        self.stop_pipeline();
        self.save();
    }
}

// ----------------------------------------------------------------------------
// main
// ----------------------------------------------------------------------------
fn usage() -> String {
    format!(
        "{APP_NAME} v{APP_VERSION}\n\nUSAGE: litecast [--profile FILE] [--record SECONDS] [--stream SECONDS]\n\n  \
         --profile FILE     use this profile JSON (default: {})\n  \
         --record SECONDS   start recording on launch, stop after SECONDS, then exit\n  \
         --stream SECONDS   go live to all enabled destinations, stop after SECONDS, then exit\n  \
         --version          print version\n  --help             this help\n",
        config_dir().join("profile.json").display()
    )
}

fn main() {
    let mut profile_path = config_dir().join("profile.json");
    let mut auto_rec: Option<Duration> = None;
    let mut auto_stream: Option<Duration> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" | "-V" => {
                println!("litecast {APP_VERSION}");
                return;
            }
            "--help" | "-h" => {
                print!("{}", usage());
                return;
            }
            "--profile" => match args.next() {
                Some(p) => profile_path = PathBuf::from(p),
                None => {
                    eprint!("--profile needs a path\n\n{}", usage());
                    std::process::exit(2);
                }
            },
            "--record" | "--stream" => match args
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| (1..=86_400 * 7).contains(v))
            {
                Some(s) if a == "--record" => auto_rec = Some(Duration::from_secs(s)),
                Some(s) => auto_stream = Some(Duration::from_secs(s)),
                None => {
                    eprint!("{a} needs seconds (1..604800)\n\n{}", usage());
                    std::process::exit(2);
                }
            },
            _ => {
                eprint!("unknown argument: {a}\n\n{}", usage());
                std::process::exit(2);
            }
        }
    }

    let fl = app::App::default().with_scheme(app::Scheme::Gtk);
    let log = Logger::default();
    log.log(format!("{APP_NAME} v{APP_VERSION} starting"));

    let prof = if profile_path.exists() {
        match load_profile(&profile_path) {
            Ok(p) => p,
            Err(e) => {
                log.log(format!("profile load failed ({e}); using defaults"));
                // v1.3.1 FIX: move the file aside only if its *content* is bad.
                // A transient read error (locked file, permissions, network
                // drive) used to rename a perfectly good profile to .bad, and
                // the next autosave then replaced it with defaults.
                if e.starts_with("invalid profile") || e.starts_with("profile too large") {
                    let bak = profile_path.with_extension("json.bad");
                    let _ = fs::rename(&profile_path, &bak);
                    log.log(format!("bad profile moved to {}", bak.display()));
                } else {
                    eprintln!("cannot read profile {}: {e}", profile_path.display());
                    std::process::exit(1);
                }
                Profile::default()
            }
        }
    } else {
        Profile::default()
    };
    log.set_secrets(secrets_of(&prof.settings));

    let q = Q(Rc::new(RefCell::new(VecDeque::new())));
    let pv = Arc::new(PreviewShared {
        buf: Mutex::new((vec![], 0, 0)),
        seq: AtomicU64::new(0),
        req: AtomicU64::new(0),
        drawn: AtomicU64::new(0),
    });
    let ov = Arc::new(Mutex::new(Overlay::default()));
    let meters: Arc<Mutex<Meters>> = Arc::default();
    // v1.3.3: palette must be set before widgets are built.
    let dark = apply_theme(&prof.settings.theme);
    if prof.settings.theme == "system" {
        log.log(format!(
            "theme: system → {}",
            if dark { "dark" } else { "light" }
        ));
    }
    let mut ui = build_ui(&q, pv.clone(), ov.clone(), meters.clone());
    ui.win.show();

    let ff = detect_ffmpeg(&prof.settings.ffmpeg_path);
    let mut st = AppState {
        prof,
        profile_path,
        ui,
        log: log.clone(),
        log_seq: 0,
        ff,
        pv: pv.clone(),
        ov,
        comp: None,
        runtimes: HashMap::new(),
        rec: None,
        stream: None,
        stream_user_on: false,
        stream_retry: None,
        stoppers: vec![],
        sel: None,
        drag: None,
        dirty: false,
        last_save: Instant::now(),
        quit: false,
        auto_record: auto_rec.map(|d| (Instant::now() + Duration::from_millis(1500), d)),
        auto_record_stop: None,
        auto_stream: auto_stream.map(|d| (Instant::now() + Duration::from_millis(1500), d)),
        auto_stream_stop: None,
        audio_hub: None,
        dsp: Arc::new(Mutex::new(AudioDsp::default())),
        meters: meters.clone(),
        rec_path: None,
    };
    st.log_ff();
    if !st.ff.ok {
        dialog::alert_default(&format!(
            "ffmpeg was not found ('{}').\nInstall ffmpeg or set its path in Controls → Settings.",
            st.prof.settings.ffmpeg_path
        ));
    }
    st.start_pipeline();
    st.refresh_scenes();
    st.refresh_sources();
    st.update_buttons();

    // Preview repaint at ~30 Hz, only when a new frame arrived. v1.3.2: the
    // compositor now waits for each draw before building the next preview,
    // so at 60 fps it builds ~30 previews/s instead of 60.
    {
        let mut preview = st.ui.preview.clone();
        let pv = pv.clone();
        let mut last = 0u64;
        app::add_timeout3(1.0 / 30.0, move |h| {
            let s = pv.seq.load(Relaxed);
            if s != last {
                last = s;
                preview.redraw();
            }
            app::repeat_timeout3(1.0 / 30.0, h);
        });
    }
    {
        let q = q.clone();
        app::add_timeout3(0.25, move |h| {
            q.send(Msg::Tick);
            app::repeat_timeout3(0.25, h);
        });
    }

    while fl.wait() {
        while let Some(m) = q.pop() {
            st.handle(m);
        }
        if st.quit {
            break;
        }
    }
    st.shutdown();
}

// ----------------------------------------------------------------------------
// Tests (pure logic only; no display needed)
// ----------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn layer(x: i32, y: i32, w: u32, h: u32, op: u8) -> Layer {
        Layer {
            x,
            y,
            w,
            h,
            opacity: op,
            slot: Arc::new(Mutex::new(None)),
            opaque: Arc::new(AtomicBool::new(true)),
        }
    }

    #[test]
    fn blit_clips_all_edges() {
        let (cw, ch) = (4usize, 4usize);
        let mut dst = [0u8, 0, 0, 255].repeat(cw * ch);
        let src = [10u8, 20, 30, 255].repeat(9);
        for (x, y) in [(-2, -2), (3, 3), (-5, 0), (10, 10), (2, -1)] {
            blit(&mut dst, cw, ch, &src, &layer(x, y, 3, 3, 255), true);
        }
        assert_eq!(&dst[0..4], &[10, 20, 30, 255]); // (-2,-2) covers (0,0)
        assert_eq!(
            &dst[(3 * cw + 3) * 4..(3 * cw + 3) * 4 + 4],
            &[10, 20, 30, 255]
        );
    }

    #[test]
    fn blit_half_opacity() {
        let mut dst = vec![0u8, 0, 0, 255];
        let src = vec![200u8, 100, 50, 255];
        blit(&mut dst, 1, 1, &src, &layer(0, 0, 1, 1, 128), true);
        assert_eq!(dst, vec![100, 50, 25, 255]);
    }

    #[test]
    fn div255_exact() {
        for x in 0..=255u32 * 255 {
            assert_eq!(div255(x), (x + 127) / 255, "x={x}");
        }
    }

    #[test]
    fn blit_rejects_short_buffer() {
        let mut dst = vec![0u8; 16];
        blit(&mut dst, 2, 2, &[1, 2, 3], &layer(0, 0, 2, 2, 255), true);
        assert_eq!(dst, vec![0u8; 16]);
    }

    #[test]
    fn sanitize_clamps_hostile_profile() {
        let mut p = Profile::default();
        p.settings.canvas_w = 999_999;
        p.settings.canvas_h = 1;
        p.settings.fps = 0;
        p.settings.encoder = "evil; rm -rf /".into();
        p.scenes[0].sources[0].w = u32::MAX;
        let dup = p.scenes[0].sources[0].clone();
        p.scenes[0].sources.push(dup); // duplicate id
        p.current = 99;
        sanitize(&mut p);
        assert_eq!(p.settings.canvas_w, MAX_W);
        assert_eq!(p.settings.canvas_h, 120);
        assert_eq!(p.settings.fps, 1);
        assert_eq!(p.settings.encoder, "libx264");
        assert_eq!(p.scenes[0].sources[0].w, MAX_W);
        assert_ne!(p.scenes[0].sources[0].id, p.scenes[0].sources[1].id);
        assert_eq!(p.current, 0);
    }

    fn dest(svc: &str, url: &str, key: &str) -> Destination {
        Destination {
            name: service_info(svc).label.into(),
            service: svc.into(),
            url: url.into(),
            key: key.into(),
            enabled: true,
        }
    }

    #[test]
    fn destination_validation_and_redaction() {
        let k = dest("kick", SERVICES[0].url, "sk_us-west-2_abc123");
        assert_eq!(
            dest_target(&k).unwrap(),
            "rtmps://fa723fc1b171.global-contribute.live-video.net/app/sk_us-west-2_abc123"
        );
        // Kick is RTMPS-only; Rumble needs a key; tee syntax chars are rejected.
        assert!(dest_target(&dest("kick", "rtmp://x/app", "k123")).is_err());
        assert!(dest_target(&dest("rumble", "rtmp://rtmp.rumble.com/live", "")).is_err());
        assert!(dest_target(&dest("custom", "rtmp://h/app|file:/etc/passwd", "")).is_err());
        assert!(dest_target(&dest("custom", "rtmp://h/app", "a[b]")).is_err());
        assert!(dest_target(&dest("custom", "http://h/app", "")).is_err());
        assert_eq!(
            dest_target(&dest("custom", "srt://h:9000?streamid=abcd", "ignored")).unwrap(),
            "srt://h:9000?streamid=abcd"
        );
        let s = Settings {
            destinations: vec![k, dest("custom", "rtmp://u:pw12@h/app", "live_xyz9")],
            ..Default::default()
        };
        let sec = secrets_of(&s);
        assert!(sec.contains(&"sk_us-west-2_abc123".to_string()));
        assert!(sec.contains(&"u:pw12".to_string()) && sec.contains(&"live_xyz9".to_string()));
        let l = Logger::default();
        l.set_secrets(sec);
        l.log("[fifo] Error opening rtmps://x/app/sk_us-west-2_abc123: I/O error");
        assert!(!l.since(0).1[0].contains("abc123"));
    }

    #[test]
    fn v1_profile_migrates_to_destination() {
        // Built explicitly: Profile::default() touches FLTK, which must not
        // run concurrently from parallel test threads.
        let json = r#"{"stream_url":"rtmp://rtmp.rumble.com/live","stream_key":"rk_42xyz"}"#;
        let mut p = Profile {
            format: 1,
            settings: serde_json::from_str(json).unwrap(),
            scenes: vec![],
            current: 0,
        };
        sanitize(&mut p);
        let d = &p.settings.destinations;
        assert_eq!(d.len(), 1);
        assert_eq!(
            (d[0].service.as_str(), d[0].key.as_str(), d[0].enabled),
            ("rumble", "rk_42xyz", true)
        );
        let out = serde_json::to_string(&p).unwrap();
        assert!(!out.contains("stream_url") && !out.contains("\"stream_key\""));
    }

    #[test]
    fn preflight_rules() {
        let ff = FfInfo {
            ok: true,
            major: 6,
            out_protocols: ["rtmp".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let mut s = Settings {
            destinations: vec![dest("kick", SERVICES[0].url, "sk_1234")],
            video_kbps: 9000,
            keyint_sec: 4,
            ..Default::default()
        };
        assert!(stream_preflight(&s, &ff).unwrap_err().contains("RTMPS"));
        let ff = FfInfo {
            out_protocols: HashSet::new(),
            ..ff
        };
        let (t, w) = stream_preflight(&s, &ff).unwrap();
        assert_eq!(t.len(), 1);
        assert!(w.iter().any(|x| x.contains("8000")) && w.iter().any(|x| x.contains("keyframe")));
        s.destinations[0].enabled = false;
        assert!(stream_preflight(&s, &ff).is_err());
    }

    #[test]
    fn encoder_args_shape() {
        let mut s = Settings {
            audio_api: "pulse".into(),
            ..Default::default()
        };
        let ff = FfInfo {
            ok: true,
            major: 6,
            minor: 1,
            ..Default::default()
        };
        let a = encoder_args(
            &s,
            &ff,
            OutKind::Record,
            &["/tmp/x.mkv".into()],
            Some("unix:/tmp/d/a.sock"),
        );
        let j = a.join(" ");
        assert!(j.contains("-f rawvideo -pix_fmt bgra"));
        // One pre-mixed input from the AudioHub; no ffmpeg-side mixing.
        assert!(j.contains("-f f32le -ar 48000 -ac 2 -i unix:/tmp/d/a.sock"));
        assert!(j.contains("-map 1:a") && !j.contains("amix") && !j.contains("pulse"));
        assert!(j.contains("-fps_mode cfr"));
        assert!(!j.contains("nal-hrd") && !j.contains("anullsrc"));
        assert!(a.last().unwrap() == "file:/tmp/x.mkv");
        // ffmpeg 4-6: input queue option on the video pipe and audio input.
        assert_eq!(j.matches("-thread_queue_size").count(), 2);
        // 7.x ignores it; current/git builds reject it on inputs (Windows
        // "Error parsing options for input file pipe:0") -> never emitted.
        for major in [7, 8, 99] {
            let j = encoder_args(
                &s,
                &FfInfo {
                    major,
                    ..ff.clone()
                },
                OutKind::Record,
                &["/tmp/x.mkv".into()],
                None,
            )
            .join(" ");
            assert!(!j.contains("thread_queue_size"), "major {major}");
        }

        // Stream, no audio, two destinations: silent track + tee/fifo fan-out.
        s.audio_api = "none".into();
        let a = encoder_args(
            &s,
            &FfInfo {
                major: 4,
                ..ff.clone()
            },
            OutKind::Stream,
            &["rtmps://k/app/key1".into(), "srt://h:1?streamid=x".into()],
            None,
        );
        let j = a.join(" ");
        assert!(
            j.contains("-re -f lavfi -i anullsrc")
                && j.contains("-map 1:a")
                && j.contains("-c:a aac")
        );
        assert!(j.contains("-vsync cfr") && j.contains("nal-hrd=cbr"));
        assert!(j.contains("-f tee -use_fifo 1") && j.contains("+global_header"));
        assert_eq!(
            a.last().unwrap(),
            "[f=flv:flvflags=no_duration_filesize:onfail=ignore]rtmps://k/app/key1|[f=mpegts:onfail=ignore]srt://h:1?streamid=x"
        );
    }

    /// Interleaved stereo sine at `amp` (peak), `secs` long.
    fn sine(freq: f32, amp: f32, secs: f32) -> Vec<f32> {
        let n = (AUD_RATE as f32 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let v =
                    amp * (2.0 * std::f32::consts::PI * freq * i as f32 / AUD_RATE as f32).sin();
                [v, v]
            })
            .collect()
    }

    /// Run a signal through a SrcDsp block by block; returns the output.
    fn run_dsp(st: &mut SrcDsp, p: &AudioDsp, sig: &[f32], duck: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(sig.len());
        for b in sig.chunks(AUD_CHUNK) {
            let mut blk = b.to_vec();
            st.process(&mut blk, p, duck);
            out.extend(blk);
        }
        out
    }

    fn tail_rms_db(x: &[f32], secs: f32) -> f32 {
        let n = ((AUD_RATE as f32 * secs) as usize * AUD_CH).min(x.len());
        block_rms_db(&x[x.len() - n..])
    }

    #[test]
    fn audio_ring_and_transport() {
        let mut r = SrcRing::default();
        let mut out = vec![1.0f32; AUD_CHUNK];
        r.push(&vec![0.5; AUD_CHUNK]);
        r.take(&mut out); // not primed (< 30 ms): silence, data kept
        assert!(out.iter().all(|&x| x == 0.0) && r.q.len() == AUD_CHUNK);
        r.push(&vec![0.5; RING_PRIME]);
        r.take(&mut out);
        assert!(out.iter().all(|&x| x == 0.5) && r.primed);
        r.q.truncate(AUD_CHUNK / 2); // underrun: zero-padded, re-prime
        r.take(&mut out);
        assert_eq!((out[0], out[AUD_CHUNK - 1], r.primed), (0.5, 0.0, false));
        r.push(&vec![0.0; RING_MAX + 1001]); // overflow trims to the cushion
        assert!(r.q.len() <= RING_PRIME + 1);
        assert_eq!(r.q.len() % AUD_CH, (RING_MAX + 1001) % AUD_CH);
        assert_eq!(pcm_bytes(&[1.0, -0.5]), [0, 0, 128, 63, 0, 0, 0, 191]);
        assert_ne!(rand_hex(), rand_hex());
        assert_eq!(rand_hex().len(), 32);

        // v1.3.1: an odd-length ring on underrun must not swap L/R afterwards.
        let mut r = SrcRing::default();
        let lr: Vec<f32> = (0..RING_PRIME)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        r.push(&lr);
        r.push(&[1.0]); // a lone L sample (partial frame)
        while r.q.len() >= AUD_CHUNK {
            r.take(&mut out);
        }
        r.take(&mut out); // underrun: whole frames only, lone L stays queued
        assert_eq!(r.q.len(), 1);
        r.push(&[-1.0]); // its R arrives
        r.push(&lr);
        r.take(&mut out);
        assert!(
            out.as_chunks::<2>()
                .0
                .iter()
                .all(|f| f[0] == 1.0 && f[1] == -1.0)
        );
        // v1.3.1: non-finite / absurd samples are neutralised on entry.
        let mut r = SrcRing::default();
        r.push(&[f32::NAN, f32::INFINITY, -1e9, 0.25]);
        assert_eq!(
            r.q.iter().copied().collect::<Vec<_>>(),
            [0.0, 0.0, -4.0, 0.25]
        );
    }

    #[test]
    fn v133_theme() {
        // Unknown or hostile theme names fall back to the default (dark).
        let mut p = Profile::default();
        assert_eq!(p.settings.theme, "dark");
        p.settings.theme = "solarized\u{0}".into();
        sanitize(&mut p);
        assert_eq!(p.settings.theme, "dark");
        for t in THEMES {
            p.settings.theme = t.to_string();
            sanitize(&mut p);
            assert_eq!(p.settings.theme, *t);
        }
        // Older profiles without the field load with the default.
        let mut j = serde_json::to_value(Profile::default()).unwrap();
        j["settings"].as_object_mut().unwrap().remove("theme");
        let old: Profile = serde_json::from_value(j).unwrap();
        assert_eq!(old.settings.theme, "dark");
        // Resolution is pure (no FLTK calls here: the FLTK palette is not
        // thread-safe and tests run in parallel). The desktop is consulted
        // only for "system", and an unknown answer means dark.
        let never = || -> Option<bool> { panic!("desktop consulted") };
        assert!(theme_is_dark("dark", never));
        assert!(!theme_is_dark("light", never));
        assert!(theme_is_dark("system", || Some(true)));
        assert!(!theme_is_dark("system", || Some(false)));
        assert!(theme_is_dark("system", || None));
        // Explicit per-widget colours differ between the two palettes.
        UI_DARK.store(false, Relaxed);
        let light = (Pal::header().to_rgb(), Pal::idle_rec().to_rgb());
        UI_DARK.store(true, Relaxed);
        assert_ne!(light, (Pal::header().to_rgb(), Pal::idle_rec().to_rgb()));
    }

    #[test]
    fn v132_efficiency() {
        // Chunk pool: steady state reuses buffers (no per-block allocation)
        // and never hands out a chunk an encoder still holds.
        let mut pool = ChunkPool::default();
        let held = pool.get(&[1.0, 2.0]); // like a lagging subscriber
        for i in 0..1000 {
            let c = pool.get(&[i as f32, -(i as f32)]);
            assert_eq!(&c[..], &pcm_bytes(&[i as f32, -(i as f32)])[..]);
        }
        assert_eq!(&held[..], &pcm_bytes(&[1.0, 2.0])[..]); // untouched
        assert!(
            pool.allocs <= ChunkPool::CAP as u64 + 2,
            "allocs {}",
            pool.allocs
        );
        assert!(pool.q.len() <= ChunkPool::CAP);
        // Encoders that keep up: one buffer is reused for ever.
        let mut pool = ChunkPool::default();
        for i in 0..1000 {
            drop(pool.get(&[i as f32, 0.0]));
        }
        assert_eq!(pool.allocs, 1);
        // A subscriber that stops reading is counted and logged, not silent.
        let log = Logger::default();
        let hub = AudioHub::start("t".into(), vec![], Arc::default(), Arc::default(), &log);
        let (_rx, drops) = hub.subscribe("rec");
        thread::sleep(Duration::from_millis(1500)); // 100-block queue fills in 1 s
        let n = drops.load(Relaxed);
        assert!((20..=60).contains(&n), "dropped blocks {n}");
        let (_, lines) = log.since(0);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("rec: encoder is not keeping up"))
        );
        drop(hub);
        // Local-time stamps are a valid time of day.
        let (h, m, s) = local_hms();
        assert!(h < 24 && m < 60 && s < 61);
    }

    #[test]
    fn v131_hardening() {
        // Recordings/screenshots are always local files, even with a URL dir.
        let s = Settings {
            audio_api: "none".into(),
            ..Settings::default()
        };
        let ff = FfInfo {
            ok: true,
            major: 7,
            ..FfInfo::default()
        };
        let a = encoder_args(
            &s,
            &ff,
            OutKind::Record,
            &["http://evil/x.mkv".into()],
            None,
        );
        assert_eq!(a.last().map(String::as_str), Some("file:http://evil/x.mkv"));
        assert_eq!(file_arg("https://cdn/a.mp4"), "https://cdn/a.mp4"); // inputs unchanged
        // Secrets are redacted even when the line is longer than the log cap.
        let log = Logger::default();
        log.set_secrets(vec!["sk_live_SECRETKEY123".into()]);
        log.log(format!("{}sk_live_SECRETKEY123", "x".repeat(590)));
        let (_, lines) = log.since(0);
        assert!(!lines[0].contains("sk_live") && !lines[0].contains("SECRET"));
        // Denormals are flushed out of the filter state during silence.
        let mut bq = Biquad::highpass(80.0);
        bq.run(1.0);
        for _ in 0..200_000 {
            bq.run(0.0);
        }
        assert_eq!((bq.z1, bq.z2), (0.0, 0.0));
    }

    #[test]
    fn audio_dsp_behaviour() {
        let off = AudioDsp {
            mic_gate: false,
            mic_comp: false,
            mic_level: false,
            desk_level: false,
            ..AudioDsp::default()
        };
        // High-pass: 20 Hz rumble cut > 10 dB, 1 kHz voice untouched.
        let mut st = SrcDsp::new(true);
        let lo = run_dsp(&mut st, &off, &sine(20.0, 0.5, 1.0), 0.0);
        assert!(tail_rms_db(&lo, 0.5) < db(0.5 / 2f32.sqrt()) - 10.0);
        let mut st = SrcDsp::new(true);
        let hi = run_dsp(&mut st, &off, &sine(1000.0, 0.5, 1.0), 0.0);
        assert!((tail_rms_db(&hi, 0.5) - db(0.5 / 2f32.sqrt())).abs() < 0.5);
        // Compressor: −6 dBFS peaks, threshold −26, 3:1 → ≈ 13 dB reduction.
        let comp = AudioDsp {
            mic_comp: true,
            comp_db: -26,
            comp_ratio: 3,
            ..off
        };
        let mut st = SrcDsp::new(true);
        let y = run_dsp(&mut st, &comp, &sine(1000.0, 0.5, 1.0), 0.0);
        let gr = db(0.5) - db(block_peak(&y[y.len() - 9600..]));
        assert!((11.0..=15.0).contains(&gr), "gain reduction {gr}");
        // Noise gate: −70 dBFS hiss with gate at −50 is pushed down ~40 dB.
        let gate = AudioDsp {
            mic_gate: true,
            gate_db: -50,
            ..off
        };
        let mut st = SrcDsp::new(true);
        let y = run_dsp(&mut st, &gate, &sine(1000.0, undb(-67.0), 1.0), 0.0);
        // Linear RMS: db() floors at −90 for display, too high to see a −110 result.
        let t = &y[y.len() - 14_400 * AUD_CH / 3..];
        let rms = (t.iter().map(|v| v * v).sum::<f32>() / t.len() as f32).sqrt();
        assert!(rms < undb(-105.0), "gated rms {rms}"); // input ≈ −70 dB, −40 dB range
        assert!(!st.gate_open);
        // Auto-level: quiet mic (−40 dB RMS) and loud desktop (−8 dB RMS)
        // both converge on the −20 dB target → balanced within 2 dB.
        let lvl = AudioDsp::default();
        let mut mic = SrcDsp::new(true);
        let mut desk = SrcDsp::new(false);
        let ym = run_dsp(&mut mic, &lvl, &sine(300.0, undb(-37.0), 12.0), 0.0);
        let yd = run_dsp(&mut desk, &lvl, &sine(300.0, undb(-5.0), 12.0), 0.0);
        let (lm, ld) = (tail_rms_db(&ym, 1.0), tail_rms_db(&yd, 1.0));
        assert!((lm + 20.0).abs() < 2.5, "mic {lm}");
        assert!((ld + 20.0).abs() < 2.5, "desktop {ld}");
        assert!((lm - ld).abs() < 2.0, "balance {lm} vs {ld}");
        // Silence is never boosted (auto-level only learns from activity).
        let mut st = SrcDsp::new(false);
        run_dsp(&mut st, &lvl, &vec![0.0; AUD_RATE * AUD_CH * 5], 0.0);
        assert_eq!(st.agc_db, 0.0);
        // Ducking: 12 dB requested → desktop ≈ 12 dB quieter.
        let mut a = SrcDsp::new(false);
        let mut b = SrcDsp::new(false);
        let x = sine(500.0, 0.3, 2.0);
        let ya = run_dsp(&mut a, &off, &x, 0.0);
        let yb = run_dsp(&mut b, &off, &x, 12.0);
        let d = tail_rms_db(&ya, 0.5) - tail_rms_db(&yb, 0.5);
        assert!((d - 12.0).abs() < 0.5, "duck {d}");
        // Mute and trim.
        let mut st = SrcDsp::new(false);
        let y = run_dsp(
            &mut st,
            &AudioDsp {
                desk_mute: true,
                ..off
            },
            &x,
            0.0,
        );
        assert!(block_peak(&y[y.len() / 2..]) == 0.0);
        // Limiter: +6 dBFS input never exceeds −1 dBFS.
        let mut lim = Limiter { g: 1.0, gr_db: 0.0 };
        let mut hot = sine(1000.0, 2.0, 0.2);
        lim.run(&mut hot, true);
        assert!(block_peak(&hot) <= undb(-1.0) + 1e-4 && lim.gr_db > 6.0);
        // Hostile profile values are clamped.
        let mut h = AudioDsp {
            target_db: 50,
            comp_ratio: 0,
            duck_db: 999,
            mic_trim_db: -500,
            ..off
        };
        h.sanitize();
        assert_eq!(
            (h.target_db, h.comp_ratio, h.duck_db, h.mic_trim_db),
            (-6, 1, 30, -30)
        );
    }

    #[test]
    fn audio_sources_and_migration() {
        let d = Settings {
            audio_api: "wasapi".into(),
            mic_device: "default".into(),
            desktop_device: "default".into(),
            ..Settings::default()
        };
        // Pre-1.2 Windows profile with empty dshow devices -> native audio.
        let mut s = Settings {
            audio_api: "dshow".into(),
            mic_device: String::new(),
            desktop_device: String::new(),
            ..d.clone()
        };
        migrate_audio(&mut s, true, &d);
        assert_eq!(
            (s.audio_api.as_str(), s.mic_device.as_str()),
            ("wasapi", "default")
        );
        // A configured dshow device is left alone; wasapi is invalid off Windows.
        let mut s = Settings {
            audio_api: "dshow".into(),
            mic_device: "Mic (USB)".into(),
            ..d.clone()
        };
        migrate_audio(&mut s, true, &d);
        assert_eq!(s.audio_api, "dshow");
        let lin = Settings {
            audio_api: "pulse".into(),
            ..d.clone()
        };
        let mut s = d.clone();
        migrate_audio(&mut s, false, &lin);
        assert_eq!(s.audio_api, "pulse");
        // The test-only lavfi backend is rejected without the env opt-in.
        if std::env::var_os("LITECAST_TEST_AUDIO").is_none() {
            let mut s = Settings {
                audio_api: "lavfi".into(),
                ..d.clone()
            };
            migrate_audio(&mut s, false, &lin);
            assert_eq!(s.audio_api, "pulse");
        }
        let ff = FfInfo {
            ok: true,
            major: 8,
            ..Default::default()
        };
        // WASAPI: mic + loopback desktop; empty field = source off.
        let v = audio_sources(&d, &ff);
        assert_eq!(v.len(), 2);
        assert!(v[0].1 && !v[1].1);
        assert_eq!(
            v[1].2,
            CapSpec::Wasapi {
                dev: "default".into(),
                loopback: true
            }
        );
        let one = Settings {
            desktop_device: String::new(),
            ..d.clone()
        };
        assert_eq!(audio_sources(&one, &ff).len(), 1);
        // Other backends: an ffmpeg capture child decoding to f32le stdout.
        let pa = Settings {
            audio_api: "pulse".into(),
            mic_device: "default".into(),
            desktop_device: "@DEFAULT_MONITOR@".into(),
            ..d.clone()
        };
        match &audio_sources(&pa, &ff)[1].2 {
            CapSpec::Ffmpeg { args, .. } => {
                let j = args.join(" ");
                assert!(j.contains("-f pulse -i @DEFAULT_MONITOR@"));
                assert!(j.ends_with("-vn -f f32le -ar 48000 -ac 2 pipe:1"));
            }
            o => panic!("unexpected {o:?}"),
        }
        let none = Settings {
            audio_api: "none".into(),
            ..pa.clone()
        };
        assert!(audio_sources(&none, &ff).is_empty());
        // No hub → record has no audio track at all.
        let j = encoder_args(&d, &ff, OutKind::Record, &["o.mkv".into()], None).join(" ");
        assert!(!j.contains("-map 1:a") && !j.contains("-c:a"));
    }

    #[test]
    fn misc_helpers() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_412), (2025, 11, 20));
        assert_eq!(parse_size("1280x720"), Some((1280, 720)));
        assert_eq!(parse_size("0x720"), None);
        assert_eq!(parse_size("99999x1"), None);
        assert_eq!(fit_rect(1920, 1080, 960, 1000), (0, 230, 960, 540));
        assert_eq!(menu_escape("/dev/video0"), "\\/dev\\/video0");
        assert_eq!(menu_escape("@DEFAULT_MONITOR@"), "@@DEFAULT_MONITOR@@");
        assert_eq!(label_escape("30 @ 60"), "30 @@ 60");
        assert_eq!(file_arg("concat:a|b"), "file:concat:a|b");
        let mut v = vec![0u8; 8];
        assert!(read_full(&mut &[1u8, 2, 3, 4, 5, 6, 7, 8][..], &mut v).unwrap());
        assert!(!read_full(&mut &[][..], &mut v).unwrap());
        assert!(read_full(&mut &[1u8, 2][..], &mut v).is_err());
    }
}
