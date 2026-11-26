use eframe::egui;
use egui::{Align2, Color32, FontDefinitions, Painter, Pos2, Rect, Stroke, Vec2};
use rusqlite::Connection;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

mod library;

// Portable paths. GPHOTOS_ROOT overrides the data root; config and cache follow
// the OS via `dirs` (XDG on Linux, known folders on Windows/macOS).
// Pure resolver: override first, then the OS base, then the temp dir.
fn app_dir(override_path: Option<&str>, base: Option<PathBuf>) -> PathBuf {
    match override_path {
        Some(p) => PathBuf::from(p),
        None => base.unwrap_or_else(std::env::temp_dir).join("gphotos-app"),
    }
}

fn config_dir() -> PathBuf {
    app_dir(None, dirs::config_dir())
}

// Systemd reads user units from the config base itself, not an app subdir.
fn config_base() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir)
}

fn systemd_unit_path(base: PathBuf) -> PathBuf {
    base.join("systemd").join("user").join(UNIT)
}

fn cache_dir() -> PathBuf {
    app_dir(None, dirs::cache_dir())
}

pub fn app_root() -> PathBuf {
    app_dir(
        std::env::var("GPHOTOS_ROOT").ok().as_deref(),
        dirs::data_dir(),
    )
}

fn gotohp_bin() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GPHOTOS_GOTOHP") {
        return std::path::PathBuf::from(p);
    }
    app_root().join("gotohp-cli_amd64")
}
const UNIT: &str = "google-unlimited.service";
const POLL_EVERY: Duration = Duration::from_secs(5);

// The sync robot is the systemd --user unit plus google-unlimited.sh plus the
// Linux gotohp-cli binary. Pure OS mapping so other platforms get an honest
// disabled state instead of a button that calls a command that isn't there.
fn linux_sync_available_on(os: &str) -> bool {
    os == "linux"
}

fn linux_sync_available() -> bool {
    linux_sync_available_on(std::env::consts::OS)
}

// Factual copy for the unavailable state: names the real requirements, claims
// no partial support or substitute. Empty when the feature is available.
const LINUX_SYNC_UNAVAILABLE: &str =
    "The sync robot is available on Linux only — it drives a systemd user service, \
     google-unlimited.sh and the Linux uploader. It is not available on this OS.";
fn linux_sync_notice(available: bool) -> &'static str {
    if available { "" } else { LINUX_SYNC_UNAVAILABLE }
}

// The "make free on this computer" path shells out to the Linux amd64 uploader.
// Storage measurement and the Colab path stay available on every OS.
const LOCAL_UPLOADER_UNAVAILABLE: &str =
    "Making items free on this computer uses a Linux-only uploader, so it is not \
     available on this OS. Choose the Colab account instead — the uploader then \
     runs inside Colab, and everything else on this page works here too.";
fn local_uploader_notice(available: bool) -> &'static str {
    if available { "" } else { LOCAL_UPLOADER_UNAVAILABLE }
}

// Pure mapping for every control that spawns the Linux uploader: enabled only
// where that uploader runs. Kept separate from the notice copy so the DISABLED
// STATE is testable on any host. The command paths still honour GPHOTOS_GOTOHP
// when called; only the UI is gated, so no dead button is left on other OSes.
fn local_uploader_control_enabled_on(os: &str) -> bool {
    linux_sync_available_on(os)
}

fn local_uploader_control_enabled() -> bool {
    local_uploader_control_enabled_on(std::env::consts::OS)
}

// hand-drawn paper palette
const PAPER: Color32 = Color32::from_rgb(0xfa, 0xf6, 0xec);
const PAPER_DARK: Color32 = Color32::from_rgb(0xf3, 0xec, 0xdd);
const INK: Color32 = Color32::from_rgb(0x2b, 0x26, 0x20);
const INK_SOFT: Color32 = Color32::from_rgb(0x8b, 0x7d, 0x6b);
const RED: Color32 = Color32::from_rgb(0xd9, 0x4f, 0x30);
const BLUE: Color32 = Color32::from_rgb(0x3b, 0x6e, 0xa5);
const GREEN: Color32 = Color32::from_rgb(0x4a, 0x7c, 0x59);
// the player's state, said the same way everywhere it appears
const STATE_ON_DISK: &str = "saved copy";
const STATE_STREAMING: &str = "streaming";
const STATE_SEEKING: &str = "seeking…";

// tile edge in points: Google Photos sized, not a 108 px postage stamp
const THUMB_CELL: f32 = 148.0;
const YELLOW: Color32 = Color32::from_rgb(0xd9, 0xa4, 0x30);
// "large files" quick chip: 100 MB, the same floor as the 100-250 MB storage bucket
const LARGE_FILE_BYTES: u64 = 100 * 1024 * 1024;
// combinable local filters. videos/photos are mutually exclusive; large stacks.
const FILTER_VIDEOS: u8 = 1;
const FILTER_PHOTOS: u8 = 2;
const FILTER_LARGE: u8 = 4;

// single-bit masks keep their historical view names (and empty-state copy);
// composites get a "filter:" name so they are recognisable as local views
fn filter_view_name(bits: u8) -> &'static str {
    match bits {
        FILTER_VIDEOS => "videos",
        FILTER_PHOTOS => "photos",
        FILTER_LARGE => "large",
        n if n & FILTER_VIDEOS != 0 => "filter:videos+large",
        _ => "filter:photos+large",
    }
}

// all active predicates must hold at once
fn filter_matches(bits: u8, is_video: bool, size: u64) -> bool {
    if bits & FILTER_VIDEOS != 0 && !is_video { return false; }
    if bits & FILTER_PHOTOS != 0 && is_video { return false; }
    if bits & FILTER_LARGE != 0 && size < LARGE_FILE_BYTES { return false; }
    true
}

#[derive(Clone, Default)]
struct Snapshot {
    watcher: bool,
    unit_installed: bool,
    account: String,
    total_uploads: u64,
    retries: u64,
    dirs: Vec<String>,
    db_mb: f64,
    today_uploads: u64,
    uploads: Vec<(String, String, u64)>,
    logs: Vec<(String, String, String)>,
}

enum Msg {
    Toast { msg: String, is_error: bool },
    Undo(String, Vec<String>),
    PickedDirs(Vec<String>),
    Token(String),
    Lib(Vec<library::MediaItem>, String, Option<String>),
    Albums(Vec<library::Album>, String),
    Links(Vec<library::SharedLink>, String),
    Things(Vec<library::Thing>, String),
    VideoUrl(String, String),
    Thumb(String, egui::ColorImage),
    RefreshLib,
    SessionReady(bool),
    // a successful browser sign-in must also reconnect the Photos session,
    // not only store the sync-robot credential
    SignedIn,
    // sign-in finished: close the helper browser and focus our window
    SignInDone,
    MoreLib(Vec<library::MediaItem>, String, Option<String>),
    Viewer(String, egui::ColorImage),
    ViewerInfo(library::BulkInfo),
    ViewerExt(String, library::ItemExt),
    Accounts(Vec<library::AccountRecord>),
    PendingAccounts(Vec<library::AccountRecord>),
    Sizes(Vec<(String, u64)>),
    StorageStats(Box<library::StorageStats>),
    AllItems(Vec<library::MediaItem>),
    BulkInfo(std::collections::HashMap<String, (String, String)>),
}

// write a file containing secrets/paths with 0600 from creation — never
// create it world-readable and chmod afterwards
fn write_secret_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path)?;
    // mode(0o600) only applies at creation; an older file may already be 0644
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(content.as_bytes())
}

fn run_cmd(cmd: &mut Command) -> (bool, String) {
    match cmd.output() {
        Ok(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            (out.status.success(), text.trim().to_string())
        }
        Err(e) => (false, format!("failed to run: {e}")),
    }
}

// Native dialogs hand back PathBufs; the action layer works in strings.
fn picked_folder(pick: Option<PathBuf>) -> Option<String> {
    pick.map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}

// Only real files are staged from a drag; directories and non-file targets
// are ignored. Pure so the drop path is testable without a GUI.
fn dropped_file_paths(files: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    files.into_iter().filter(|p| p.is_file()).collect()
}

fn picked_files(pick: Option<Vec<PathBuf>>) -> Vec<String> {
    pick.unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

// curl's config parser needs a real device name, and it differs on Windows.
fn null_device(windows: bool) -> &'static str {
    if windows { "NUL" } else { "/dev/null" }
}

// Unix seconds -> SystemTime; negative times stay representable.
fn timestamp_from_unix(ts: i64) -> Option<std::time::SystemTime> {
    if ts < 0 {
        std::time::UNIX_EPOCH.checked_sub(Duration::from_secs(ts.unsigned_abs()))
    } else {
        std::time::UNIX_EPOCH.checked_add(Duration::from_secs(ts as u64))
    }
}

// On Linux the clipboard contents die with the last Clipboard, so a per-copy
// owner would drop them before the user can paste. Keep one for the process;
// the mutex also serialises operations that would otherwise fight over it.
static CLIPBOARD: std::sync::OnceLock<std::sync::Mutex<arboard::Clipboard>> =
    std::sync::OnceLock::new();

fn clipboard() -> Option<&'static std::sync::Mutex<arboard::Clipboard>> {
    if CLIPBOARD.get().is_none() {
        if let Ok(cb) = arboard::Clipboard::new() {
            let _ = CLIPBOARD.set(std::sync::Mutex::new(cb));
        }
    }
    CLIPBOARD.get()
}

fn copy_text_to_clipboard(text: &str) -> bool {
    let Some(clip) = clipboard() else {
        return false;
    };
    let mut cb = clip.lock().unwrap_or_else(|e| e.into_inner());
    cb.set_text(text).is_ok()
}

fn db_query(sql: &str) -> Vec<Vec<String>> {
    let path = app_root().join("gphotos.db");
    if !path.exists() {
        return vec![];
    }
    let conn = match Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let ncols = stmt.column_count();
    let mut rows = vec![];
    if let Ok(mut results) = stmt.query([]) {
        while let Ok(Some(row)) = results.next() {
            let mut vals = vec![];
            for i in 0..ncols {
                vals.push(
                    row.get_ref(i)
                        .map(|v| match v {
                            rusqlite::types::ValueRef::Null => String::new(),
                            rusqlite::types::ValueRef::Integer(x) => x.to_string(),
                            rusqlite::types::ValueRef::Real(x) => x.to_string(),
                            rusqlite::types::ValueRef::Text(t) => {
                                String::from_utf8_lossy(t).to_string()
                            }
                            rusqlite::types::ValueRef::Blob(b) => {
                                String::from_utf8_lossy(b).to_string()
                            }
                        })
                        .unwrap_or_default(),
                );
            }
            rows.push(vals);
        }
    }
    rows
}

fn read_dirs() -> Vec<String> {
    match fs::read_to_string(app_root().join("watch_dirs.txt")) {
        Ok(text) => text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect(),
        Err(_) => vec![],
    }
}

fn watcher_active() -> bool {
    if !linux_sync_available() { return false; }
    let (ok, out) = run_cmd(
        Command::new("systemctl").arg("--user").arg("is-active").arg(UNIT),
    );
    ok && out == "active"
}

fn unit_installed() -> bool {
    systemd_unit_path(config_base()).exists()
}

fn account() -> String {
    if !linux_sync_available() { return String::new(); }
    let (ok, out) = run_cmd(Command::new(gotohp_bin()).arg("creds").arg("list"));
    let low = out.to_lowercase();
    if ok && !low.contains("no credentials") && !out.is_empty() {
        out.lines().next().unwrap_or("").to_string()
    } else {
        String::new()
    }
}

fn poll() -> Snapshot {
    let uploads: Vec<(String, String, u64)> = db_query(
        "SELECT uploaded_at, file_name, file_size FROM uploads ORDER BY uploaded_at DESC LIMIT 25",
    )
    .into_iter()
    .filter_map(|r| {
        let size = r.get(2).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        Some((r.get(0)?.clone(), r.get(1)?.clone(), size))
    })
    .collect();
    let logs: Vec<(String, String, String)> = db_query(
        "SELECT timestamp, level, message FROM logs ORDER BY id DESC LIMIT 60",
    )
    .into_iter()
    .filter_map(|r| Some((r.get(0)?.clone(), r.get(1)?.clone(), r.get(2)?.clone())))
    .collect();
    let db_mb = fs::metadata(app_root().join("gphotos.db"))
        .map(|m| m.len() as f64 / 1048576.0)
        .unwrap_or(0.0);
    let today_uploads = db_query(
        "SELECT COUNT(*) FROM uploads WHERE uploaded_at >= date('now','localtime')",
    )
    .first()
    .and_then(|r| r.first().and_then(|v| v.parse::<u64>().ok()))
    .unwrap_or(0);

    Snapshot {
        watcher: watcher_active(),
        unit_installed: unit_installed(),
        account: account(),
        total_uploads: db_query("SELECT COUNT(*) FROM uploads")
            .first()
            .and_then(|r| r.first().and_then(|v| v.parse::<u64>().ok()))
            .unwrap_or(0),
        retries: db_query("SELECT COUNT(*) FROM retry_queue")
            .first()
            .and_then(|r| r.first().and_then(|v| v.parse::<u64>().ok()))
            .unwrap_or(0),
        today_uploads,
        dirs: read_dirs(),
        db_mb: (db_mb * 100.0).round() / 100.0,
        uploads,
        logs,
    }
}

// ---------------------------------------------------------------------------
// sketchy drawing helpers — deterministic wobble so nothing flickers
// ---------------------------------------------------------------------------

fn jitter(seed: u32) -> f32 {
    // deterministic pseudo-random in [-1, 1]
    ((seed as f32 * 12.9898).sin() * 43758.5453).fract() * 2.0 - 1.0
}

fn sketchy_line(p: &Painter, a: Pos2, b: Pos2, color: Color32, width: f32, seed: u32) {
    sketchy_line_wavy(p, a, b, color, width, seed, 0.0)
}

/// sketchy line with a gentle travelling wave; t = seconds (0 = static)
fn sketchy_line_wavy(p: &Painter, a: Pos2, b: Pos2, color: Color32, width: f32, seed: u32, t: f32) {
    let dist = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
    let segs = (dist / 14.0).ceil().max(2.0) as u32;
    let dir = Vec2::new(b.x - a.x, b.y - a.y);
    let len = dir.length().max(1.0);
    let perp = Vec2::new(-dir.y / len, dir.x / len);
    let mut prev = a;
    for i in 1..=segs {
        let f = i as f32 / segs as f32;
        let base = Pos2::new(a.x + dir.x * f, a.y + dir.y * f);
        let off = jitter(seed.wrapping_add(i)) * 2.2
            + if t > 0.0 { (t * 2.0 + i as f32 * 0.9).sin() * 1.3 } else { 0.0 };
        let pt = if i == segs { b } else { base + perp * off };
        p.line_segment([prev, pt], Stroke::new(width, color));
        prev = pt;
    }
}

fn sketchy_polyline(p: &Painter, pts: &[Pos2], color: Color32, width: f32, seed: u32) {
    for w in pts.windows(2) {
        sketchy_line(p, w[0], w[1], color, width, seed);
    }
}

fn sketchy_rect(p: &Painter, r: Rect, color: Color32, width: f32, seed: u32) {
    let (tl, tr, br, bl) = (
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
    );
    sketchy_line(p, tl, tr, color, width, seed);
    sketchy_line(p, tr, br, color, width, seed.wrapping_add(101));
    sketchy_line(p, br, bl, color, width, seed.wrapping_add(202));
    sketchy_line(p, bl, tl, color, width, seed.wrapping_add(303));
}

fn sketchy_circle(p: &Painter, c: Pos2, radius: f32, color: Color32, width: f32, seed: u32) {
    let n = 26;
    let mut prev = Pos2::new(c.x + radius, c.y);
    for i in 1..=n {
        let ang = (i as f32) / n as f32 * std::f32::consts::TAU;
        let wob = 1.0 + jitter(seed.wrapping_add(i)) * 0.04;
        let pt = Pos2::new(c.x + ang.cos() * radius * wob, c.y + ang.sin() * radius * wob);
        p.line_segment([prev, pt], Stroke::new(width, color));
        prev = pt;
    }
}

/// hand-drawn logo: wobbly photo stack with mountain, sun and cloud
fn draw_logo(p: &Painter, origin: Pos2, scale: f32, t: f32) {
    let s = scale;
    macro_rules! rel {
        ($dx:expr, $dy:expr) => {
            Pos2::new(origin.x + ($dx as f32) * s, origin.y + ($dy as f32) * s)
        };
    }

    // back photo peeking out top-right, tilted the other way
    let back = [
        rel!(28, 10),
        rel!(74, 16),
        rel!(70, 54),
        rel!(24, 48),
    ];
    p.add(egui::Shape::convex_polygon(
        back.to_vec(),
        Color32::from_rgb(0xf3, 0xe7, 0xd3),
        Stroke::NONE,
    ));
    sketchy_polyline(p, &back, INK_SOFT, 3.0, 11);
    sketchy_line(p, back[3], back[0], INK_SOFT, 3.0, 507);

    // front photo — the main card
    let front = [
        rel!(12, 26),
        rel!(72, 22),
        rel!(76, 78),
        rel!(16, 82),
    ];
    p.add(egui::Shape::convex_polygon(
        front.to_vec(),
        Color32::from_rgb(0xff, 0xfd, 0xf5),
        Stroke::NONE,
    ));
    sketchy_polyline(p, &front, INK, 3.5, 77);
    sketchy_line(p, front[3], front[0], INK, 3.5, 877);

    // mountains inside the photo
    sketchy_polyline(
        p,
        &[
            rel!(20, 74),
            rel!(38, 48),
            rel!(48, 62),
            rel!(60, 42),
            rel!(70, 72),
        ],
        BLUE,
        3.0,
        33,
    );
    // little pine on the slope
    sketchy_polyline(
        p,
        &[rel!(30, 76), rel!(36, 62), rel!(42, 76), rel!(30, 76)],
        GREEN,
        2.5,
        55,
    );
    // sun inside the photo, top-right corner
    sketchy_circle(p, rel!(58, 38), 6.0 * s, RED, 3.0, 99);
    for i in 0..8 {
        let a = i as f32 / 8.0 * std::f32::consts::TAU + t * 0.35;
        let r1 = 9.0 * s;
        let r2 = 12.0 * s;
        sketchy_line(
            p,
            rel!(58.0 + a.cos() * r1, 38.0 + a.sin() * r1),
            rel!(58.0 + a.cos() * r2, 38.0 + a.sin() * r2),
            RED,
            2.5,
            200 + i,
        );
    }

    // drifting, bobbing cloud above the photo — clear of the sun
    let bob = (t * 2.2).sin() * 3.5;
    let drift = (t * 0.9).sin() * 5.0;
    let cloud = [
        Pos2::new(origin.x + (22.0 + drift) * s, origin.y + (16.0 + bob) * s),
        Pos2::new(origin.x + (18.0 + drift) * s, origin.y + (11.0 + bob) * s),
        Pos2::new(origin.x + (25.0 + drift) * s, origin.y + (5.0 + bob) * s),
        Pos2::new(origin.x + (33.0 + drift) * s, origin.y + (9.0 + bob) * s),
        Pos2::new(origin.x + (40.0 + drift) * s, origin.y + (3.0 + bob) * s),
        Pos2::new(origin.x + (46.0 + drift) * s, origin.y + (9.0 + bob) * s),
        Pos2::new(origin.x + (52.0 + drift) * s, origin.y + (6.0 + bob) * s),
        Pos2::new(origin.x + (54.0 + drift) * s, origin.y + (12.0 + bob) * s),
        Pos2::new(origin.x + (47.0 + drift) * s, origin.y + (15.0 + bob) * s),
        Pos2::new(origin.x + (39.0 + drift) * s, origin.y + (16.0 + bob) * s),
        Pos2::new(origin.x + (30.0 + drift) * s, origin.y + (17.0 + bob) * s),
    ];
    p.add(egui::Shape::convex_polygon(
        cloud.to_vec(),
        Color32::from_rgb(0xff, 0xff, 0xff),
        Stroke::NONE,
    ));
    sketchy_polyline(p, &cloud, INK, 3.0, 404);
    sketchy_line(p, cloud[10], cloud[0], INK, 3.0, 540);
}

// ---------------------------------------------------------------------------

// An account is identified by the browser profile that holds it AND its email.
type AccountKey = (String, String);

struct AddPoll {
    cancel: Arc<AtomicBool>,
    jar: String,
}

// Pure OS mapping for handing a URL or a path to the desktop.
fn url_open_command(target: &str) -> (&'static str, Vec<String>) {
    #[cfg(target_os = "windows")]
    {
        ("cmd", vec!["/C".into(), "start".into(), String::new(), target.into()])
    }
    #[cfg(target_os = "macos")]
    {
        ("open", vec![target.into()])
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        ("xdg-open", vec![target.into()])
    }
}

fn open_url(url: &str) {
    let _ = spawn_open_url(url);
}

// Like open_url, but returns the child so sign-in can close the window it
// opened. The OS opener may exit immediately after handing off to a running
// browser; in that case there is little to close, which is acceptable.
fn spawn_open_url(url: &str) -> Option<std::process::Child> {
    let (prog, args) = url_open_command(url);
    Command::new(prog).args(&args).spawn().ok()
}

// Build the browser argv that opens `url` in the profile discovery inspects.
// A jar is "browser|profile|cookie-db-path" (gcookies.py emits three fields);
// field 1 is the profile dir, which may be nested like "Brave-Origin/Default" —
// Chrome/Brave want only its last component. Non-browser labels (e.g.
// "imported|...") and unknown browsers return None so the caller falls back to
// the OS URL helper. Pure so the argv can be asserted in tests.
fn jar_browser_argv(jar: &str, url: &str) -> Option<(String, Vec<String>)> {
    let browser = jar.split('|').next().unwrap_or("");
    let profile = jar.split('|').nth(1).unwrap_or("");
    let profile = profile.rsplit('/').next().unwrap_or("");
    // Real executable names per OS — the Linux-only list used to make the
    // "add account" profile picker a no-op on Windows and macOS.
    let exe = match (std::env::consts::OS, browser) {
        ("windows", "firefox") => "firefox.exe",
        ("windows", "chromium") => "chrome.exe",
        ("windows", "google-chrome" | "chrome") => "chrome.exe",
        ("windows", "Brave-Browser" | "Brave-Origin" | "brave") => "brave.exe",
        ("macos", "firefox") => "Firefox",
        ("macos", "chromium") => "Chromium",
        ("macos", "google-chrome" | "chrome") => "Google Chrome",
        ("macos", "Brave-Browser" | "Brave-Origin" | "brave") => "Brave Browser",
        (_, "firefox") => "firefox",
        (_, "chromium") => "chromium",
        (_, "google-chrome" | "chrome") => "google-chrome",
        (_, "Brave-Browser" | "Brave-Origin" | "brave") => "brave-browser",
        _ => return None,
    };
    let mut args = Vec::new();
    // Firefox needs -P <profile>; Chromium-family uses --profile-directory.
    if !profile.is_empty() {
        if browser == "firefox" {
            args.push("-P".into());
            args.push(profile.to_string());
        } else {
            args.push(format!("--profile-directory={profile}"));
        }
    }
    args.push(url.to_string());
    Some((exe.to_string(), args))
}

// Open the sign-in page in the profile discovery will inspect.
fn open_jar_url(jar: &str, url: &str) {
    if let Some((exe, args)) = jar_browser_argv(jar, url) {
        if Command::new(&exe).args(&args).spawn().is_ok() {
            return;
        }
    }
    open_url(url);
}

struct App {
    snap: Arc<Mutex<Snapshot>>,
    snap_dirty: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    accounts: Arc<Mutex<Vec<String>>>,
    rx: mpsc::Receiver<Msg>,
    tx: mpsc::Sender<Msg>,
    dirs_text: String,
    token: String,
    pending_toasts: Vec<(String, bool, Instant)>,
    // last reversible bulk action: (inverse action, dedup keys, time)
    undo: Option<(String, Vec<String>, Instant)>,
    // speed & limits settings (persisted in gu.config / immich.config)
    delay_s: String,
    cap: String,
    immich_url: String,
    immich_key: String,
    immich_tag: String,
    // upload extras
    album: String,
    delete_after: bool,
    // library (Google Photos Toolkit port)
    lib_items: Arc<Mutex<Vec<library::MediaItem>>>,
    lib_albums: Arc<Mutex<Vec<library::Album>>>,
    lib_links: Arc<Mutex<Vec<library::SharedLink>>>,
    lib_things: Arc<Mutex<Vec<library::Thing>>>,
    lib_selected: Arc<Mutex<HashSet<String>>>,
    // albums have their own selection: a tile is not a photo
    album_selected: HashSet<String>,
    confirm_album_del: bool,
    album_target: Option<String>,
    album_sort: u8,
    sel_anchor: Option<String>,
    jump_hook: Option<String>,
    date_labels: std::collections::HashMap<i64, std::sync::Arc<str>>,
    // bursts view: media_key -> (group index, group size). Drives both the
    // per-tile "×N" badge and the per-group header (built from each group's
    // first item). UI-thread only — no worker needs it, so no Arc/Mutex.
    burst_info: std::collections::HashMap<String, (usize, usize)>,
    // the grid snapshot is rebuilt only when the underlying list changes
    items_gen: Arc<AtomicU64>,
    grid_cache: Arc<Vec<library::MediaItem>>,
    grid_cache_gen: u64,
    // the right-hand rail drives the grid ScrollArea directly: dragging sets
    // a fraction and we force the matching offset before layout, no animation
    grid_frac: f32,
    grid_drag: bool,
    grid_view_h: f32,
    grid_content_h: f32,
    grid_want: Option<f32>,
    grid_sort: u8,
    hog_year: Option<i64>,
    // RAM thumb cache is capped: the disk cache is authoritative and a tile
    // keeps its GPU texture even if its ColorImage is evicted here
    thumb_order: std::collections::VecDeque<String>,
    sorted_cache: Vec<library::MediaItem>,
    sorted_cache_key: (u64, u8),
    // grid snapshots keyed by view name, so "< back" from a local subset can
    // restore the previous grid instead of calling fetch_lib with a local name
    view_snapshots: Arc<Mutex<std::collections::HashMap<String, Vec<library::MediaItem>>>>,
    // filenames + captions from the one-time scan, for local-only search
    bulk: Arc<Mutex<std::collections::HashMap<String, (String, String)>>>,
    // the date rail is rebuilt only when the measured list changes, not every frame
    // keys whose CDN refused a Range request: seek by finishing the local copy
    non_seekable: Arc<Mutex<HashSet<String>>>,
    pending_seek: Option<(String, f64, u64)>,
    pending_since: Option<Instant>,
    last_warm: Instant,
    warm_active: bool,
    month_cache: Vec<MonthBucket>,
    month_cache_key: usize,
    // controls / playback preferences
    ctl_hide_secs: f64,
    confirm_del_setting: bool,
    autoplay: bool,
    loop_default: bool,
    seek_step: f64,
    copy_links: bool,
    show_storage: bool,
    slideshow_secs: f64,
    slideshow_at: Instant,
    lib_busy: Arc<AtomicBool>,
    lib_session: Arc<Mutex<Option<library::Session>>>,
    lib_search: String,
    album_name: String,
    lib_view: Arc<Mutex<String>>,
    lib_next: Arc<Mutex<Option<String>>>,
    page: Arc<Mutex<String>>,
    thumb_filling: Arc<AtomicBool>,
    tex_budget: usize,
    confirm_delete: bool,
    quota_txt: Arc<Mutex<String>>,
    // storage: which items Google actually charges for, and the list of them
    storage_open: bool,
    storage_busy: Arc<AtomicBool>,
    storage_rows: Arc<Mutex<Vec<StorageRow>>>,
    storage_note: Arc<Mutex<String>>,
    thumbs: Arc<Mutex<std::collections::HashMap<String, egui::ColorImage>>>,
    tex: std::collections::HashMap<String, egui::TextureHandle>,
    start: Instant,
    // full-screen photo viewer
    viewer: Option<String>,
    viewer_img: Arc<Mutex<Option<(String, egui::ColorImage)>>>,
    viewer_zoom: f32,
    viewer_info_open: bool,
    viewer_info: Arc<Mutex<Option<library::BulkInfo>>>,
    viewer_ext: Arc<Mutex<Option<library::ItemExt>>>,
    viewer_confirm_del: bool,
    viewer_desc: String,
    viewer_date: String,
    caption_loaded_for: Option<String>,
    // account switcher — identity is (jar, email), never a mutable authuser slot
    switch_open: bool,
    switch_list: Arc<Mutex<Vec<library::AccountRecord>>>,
    selected: Option<AccountKey>,
    hidden: HashSet<AccountKey>,
    // add/create flow: chosen profile, active detection, staged Add prompts
    add_jar: String,
    add_poll: Option<AddPoll>,
    pending_adds: Vec<library::AccountRecord>,
    // Windows ABE fallback: path to a local cookie-export file to import
    cookie_import_path: String,
    // per-item file sizes (media_key -> bytes) for the storage-hogs view
    sizes: Arc<Mutex<std::collections::HashMap<String, u64>>>,
    charged: Arc<Mutex<std::collections::HashSet<String>>>,
    hog_min_mb: String,
    // what the storage page measured, once the whole library has been read
    hog_stats: Option<library::StorageStats>,
    // the measured list, kept so opening "biggest items" is instant after boot
    all_items: Arc<Mutex<Vec<library::MediaItem>>>,
    // combinable local filter chips: bit 1 videos, bit 2 photos, bit 4 large
    filter_bits: u8,
    scan_started: Arc<AtomicBool>,
    // bumped on account switch so in-flight workers from the old account
    // cannot inject their data into the new one
    account_epoch: Arc<AtomicU64>,
    // anti-double-fire guard for swipe/scroll photo navigation
    last_nav: Instant,
    // where the back button goes
    view_prev: Arc<Mutex<String>>,
    // in-app video playback (ffmpeg frames + cpal audio)
    vid: Option<Vid>,
    // a decoder already running for the tile the pointer is on, so a click
    // does not pay the ~2 s the cdn takes to send the first frame
    prewarm: Option<Prewarm>,
    // resolved stream urls (media_key -> url, when) — resolving costs ~0.8 s
    vid_urls: Arc<Mutex<std::collections::HashMap<String, (String, Instant)>>>,
    // media_keys whose url is being resolved right now
    resolving: Arc<Mutex<HashSet<String>>>,
    // throttle for the silent next-page fetch
    last_more: Instant,
    // test hook: GPHOTOS_SHOT=/path grabs a png from inside the app, so checks
    // work while the window is behind the user's other windows
    shot_path: Option<String>,
    shot_at: Instant,
    // settings: cache thumbs / full photos / videos on this computer
    cache_on: bool,
    // settings: show the locked folder entry in the sidebar
    locked_in_ui: bool,
    // coming back from the viewer: the grid scrolls to the item you were on
    scroll_anchor: Option<String>,
    // jump-to-date: the second a typed date resolves to, waiting for its page
    jump_target: Option<i64>,
    jump_open: bool,
    jump_text: String,
    // the month rail on the right edge of the timeline
    rail_on: bool,
    // video: where each file was left off, and the seek-bar hover preview
    resume: std::collections::HashMap<String, f64>,
    resume_ok: bool,
    resume_wrote: Instant,
    seek_thumb: Option<SeekThumb>,
    // controls reappear when the pointer comes near, or right after a key
    ctl_touch: Instant,
    // VLC-style chrome: hide 2 s after the mouse stops, wake on movement or
    // when playback starts
    ctl_move: Instant,
    last_pointer: Option<Pos2>,
    // "resumed from 12:34" strip, up for a few seconds after a resume
    resume_note: Option<(String, f64, Instant)>,
    // hover previews: which video tile the pointer is on, and its player
    hover_want: Option<(String, String)>,
    hover_prev: Option<String>,
    hover_since: Instant,
    hover_vid: Option<HoverVid>,
    // the keyboard card: what every key does, on `?`
    show_keys: bool,
    // look: edge of one thumbnail in the grid
    thumb_size: f32,
    // uploads: free (no storage used), saver (smaller file), quota (full quality)
    upload_mode: String,
    // files staged for upload from drag-and-drop or the picker; nothing is
    // uploaded until the user presses Start in the popup
    upload_queue: Vec<std::path::PathBuf>,
    upload_popup: bool,
    // where a storage job runs: this computer, or the signed-in Colab account
    engine: String,
    // play videos with sound instead of muted
    video_sound: bool,
    // viewer: the window is in fullscreen, so closing it has to restore
    fullscreen: bool,
    restore_window: bool,
    cache_sizes: Option<(u64, u64, u64)>,
    // launch deeplink, applied once the library is ready
    pending_deeplink: Option<(Kind, String)>,
}

fn read_cfg(name: &str) -> Vec<(String, String)> {
    match fs::read_to_string(app_root().join(name)) {
        Ok(text) => text
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                if l.is_empty() || l.starts_with('#') {
                    return None;
                }
                l.split_once('=').map(|(k, v)| {
                    (
                        k.trim().to_string(),
                        v.trim().trim_matches('"').to_string(),
                    )
                })
            })
            .collect(),
        Err(_) => vec![],
    }
}

fn cfg_get(cfg: &[(String, String)], key: &str) -> String {
    cfg.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

impl App {
    fn load_settings(&mut self) {
        let gu = read_cfg("gu.config");
        self.delay_s = cfg_get(&gu, "GU_DELAY_SECONDS");
        self.cap = cfg_get(&gu, "GU_DAILY_CAP");
        self.album = cfg_get(&gu, "GU_ALBUM");
        self.delete_after = cfg_get(&gu, "GU_DELETE_AFTER") == "1";
        let im = read_cfg("immich.config");
        self.immich_url = cfg_get(&im, "GU_IMMICH_URL");
        self.immich_key = cfg_get(&im, "GU_IMMICH_API_KEY");
        self.immich_tag = if cfg_get(&im, "GU_IMMICH_TAG").is_empty() {
            "gphotos-synced".to_string()
        } else {
            cfg_get(&im, "GU_IMMICH_TAG")
        };
    }

    fn save_settings(
        &self,
        delay_s: String,
        cap: String,
        immich_url: String,
        immich_key: String,
        immich_tag: String,
        album: String,
        delete_after: bool,
    ) {
        self.spawn_action(move |tx| {
            let gu = format!(
                "# written by Google Photos app\nGU_DELAY_SECONDS={}\nGU_DAILY_CAP={}\nGU_ALBUM={}\nGU_DELETE_AFTER={}\n",
                delay_s.trim(),
                cap.trim(),
                album.trim(),
                if delete_after { "1" } else { "0" }
            );
            let im = format!(
                "# written by Google Photos app\nGU_IMMICH_URL={}\nGU_IMMICH_API_KEY={}\nGU_IMMICH_TAG={}\n",
                immich_url.trim(),
                immich_key.trim(),
                immich_tag.trim()
            );
            let w1 = write_secret_file(&app_root().join("gu.config"), &gu);
            let w2 = write_secret_file(&app_root().join("immich.config"), &im);
            let ok = w1.is_ok() && w2.is_ok();
            let _ = tx.send(Msg::Toast {
                msg: if ok {
                    "speed & limits saved".into()
                } else {
                    "could not write config files".into()
                },
                is_error: !ok,
            });
            if ok && watcher_active() {
                // reload configs into the running watcher
                let _ = run_cmd(Command::new("systemctl").arg("--user").arg("restart").arg(UNIT));
            }
        });
    }
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        deeplink: Option<(Kind, String)>,
        bad_link: bool,
    ) -> Self {
        set_fonts(&cc.egui_ctx);
        set_theme(&cc.egui_ctx);

        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let snap_dirty = Arc::new(AtomicBool::new(true));
        let busy = Arc::new(AtomicBool::new(false));
        let accounts = Arc::new(Mutex::new(vec![]));
        let (tx, rx) = mpsc::channel();
        // tell the user once if the optional media tools are missing — a dead
        // video player with no explanation is worse than an honest warning
        let mut missing = Vec::new();
        if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| !s.success()).unwrap_or(true) {
            missing.push("ffmpeg");
        }
        // audio goes straight to the device through cpal; paplay is not needed
        if !missing.is_empty() {
            let _ = tx.send(Msg::Toast {
                msg: format!("not installed: {} — video playback needs {}", missing.join(", "), if missing.len() == 1 { "it" } else { "them" }),
                is_error: true,
            });
        }
        // same for the bundled helper scripts: without them sign-in and every
        // image/video URL fail silently, which looks like an empty library
        let missing_helpers: Vec<&str> = ["gcookies.py", "fifecookies.py"]
            .into_iter()
            .filter(|f| !app_root().join(f).is_file())
            .collect();
        if !missing_helpers.is_empty() {
            let _ = tx.send(Msg::Toast {
                msg: format!(
                    "missing {} in {} — copy them from companion/ (see companion/README.md)",
                    missing_helpers.join(", "),
                    app_root().display()
                ),
                is_error: true,
            });
        }

        let (snap2, dirty2) = (snap.clone(), snap_dirty.clone());
        thread::spawn(move || loop {
            let s = poll();
            if let Ok(mut g) = snap2.lock() {
                *g = s;
            }
            dirty2.store(true, Ordering::Relaxed);
            thread::sleep(POLL_EVERY);
        });

        let mut app = Self {
            snap,
            snap_dirty,
            busy,
            accounts,
            rx,
            tx,
            dirs_text: read_dirs().join("\n"),
            token: String::new(),
            pending_toasts: vec![],
            undo: None,
            delay_s: String::new(),
            cap: String::new(),
            immich_url: String::new(),
            immich_key: String::new(),
            immich_tag: "gphotos-synced".into(),
            album: String::new(),
            delete_after: false,
            lib_items: Arc::new(Mutex::new(vec![])),
            lib_albums: Arc::new(Mutex::new(vec![])),
            lib_links: Arc::new(Mutex::new(vec![])),
            lib_things: Arc::new(Mutex::new(vec![])),
            lib_selected: Arc::new(Mutex::new(HashSet::new())),
            album_selected: Default::default(),
            confirm_album_del: false,
            album_target: None,
            album_sort: setting("album_sort", "0").parse().ok().filter(|v: &u8| *v < 3).unwrap_or(0),
            sel_anchor: None,
            jump_hook: None,
            date_labels: Default::default(),
            burst_info: Default::default(),
            items_gen: Arc::new(AtomicU64::new(0)),
            grid_cache: Arc::new(vec![]),
            grid_cache_gen: u64::MAX,
            non_seekable: Arc::new(Mutex::new(HashSet::new())),
            pending_seek: None,
            pending_since: None,
            last_warm: Instant::now(),
            warm_active: false,
            grid_frac: 0.0,
            grid_drag: false,
            grid_view_h: 0.0,
            grid_content_h: 0.0,
            grid_want: None,
            grid_sort: setting("grid_sort", "0").parse().ok().filter(|v: &u8| *v < 4).unwrap_or(0),
            hog_year: None,
            thumb_order: Default::default(),
            sorted_cache: vec![],
            sorted_cache_key: (u64::MAX, u8::MAX),
            view_snapshots: Arc::new(Mutex::new(Default::default())),
            bulk: Arc::new(Mutex::new(std::collections::HashMap::new())),
            month_cache: vec![],
            month_cache_key: usize::MAX,
            ctl_hide_secs: setting("ctl_hide", "2")
                .parse::<f64>()
                .unwrap_or(2.0)
                .clamp(0.5, 10.0),
            confirm_del_setting: setting("confirm_del", "1") != "0",
            autoplay: setting("autoplay", "1") != "0",
            loop_default: setting("loop", "0") == "1",
            seek_step: setting("seek_step", "10")
                .parse::<f64>()
                .unwrap_or(10.0)
                .clamp(1.0, 120.0),
            copy_links: setting("copy_links", "1") != "0",
            show_storage: setting("storage_sidebar", "1") != "0",
            slideshow_secs: setting("slideshow", "0")
                .parse::<f64>()
                .unwrap_or(0.0)
                .clamp(0.0, 30.0),
            slideshow_at: Instant::now(),
            lib_busy: Arc::new(AtomicBool::new(false)),
            lib_session: Arc::new(Mutex::new(None)),
            lib_search: String::new(),
            album_name: String::new(),
            lib_view: Arc::new(Mutex::new("recent".into())),
            lib_next: Arc::new(Mutex::new(None)),
            view_prev: Arc::new(Mutex::new("recent".into())),
            vid: None,
            prewarm: None,
            vid_urls: Arc::new(Mutex::new(std::collections::HashMap::new())),
            resolving: Arc::new(Mutex::new(HashSet::new())),
            last_more: Instant::now(),
            shot_path: std::env::var("GPHOTOS_SHOT").ok(),
            shot_at: Instant::now(),
            cache_on: load_cache_flag(),
            // the locked folder is a settings switch now, not a sidebar entry
            locked_in_ui: setting("locked_in_ui", "0") != "0",
            scroll_anchor: None,
            jump_target: None,
            jump_open: std::env::var("GPHOTOS_JUMPDATE").is_ok(),
            jump_text: std::env::var("GPHOTOS_JUMPTEXT").unwrap_or_default(),
            rail_on: setting("rail", "1") != "0",
            resume: load_resume(),
            resume_ok: setting("resume", "1") != "0",
            resume_wrote: Instant::now(),
            seek_thumb: None,
            ctl_touch: Instant::now() - Duration::from_secs(60),
            ctl_move: Instant::now(),
            last_pointer: None,
            resume_note: None,
            hover_want: None,
            hover_prev: None,
            hover_since: Instant::now(),
            hover_vid: None,
            show_keys: std::env::var("GPHOTOS_KEYS").is_ok(),
            thumb_size: setting("thumb_size", "148")
                .parse::<f32>()
                .unwrap_or(148.0)
                .clamp(96.0, 220.0),
            engine: {
                let e = setting("engine", "local");
                if e == "colab" { e } else { "local".into() }
            },
            upload_queue: Vec::new(),
            upload_popup: false,
            upload_mode: {
                let m = setting("upload_mode", "free");
                if m == "saver" || m == "quota" { m } else { "free".into() }
            },
            video_sound: setting("video_sound", "1") != "0",
            fullscreen: false,
            restore_window: false,
            cache_sizes: None,
            page: Arc::new(Mutex::new("photos".into())),
            thumb_filling: Arc::new(AtomicBool::new(false)),
            tex_budget: 0,
            confirm_delete: false,
            quota_txt: Arc::new(Mutex::new(String::new())),
            storage_open: std::env::var("GPHOTOS_STORAGE").is_ok(),
            storage_busy: Arc::new(AtomicBool::new(false)),
            storage_rows: Arc::new(Mutex::new(Vec::new())),
            storage_note: Arc::new(Mutex::new(String::new())),
            thumbs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            tex: std::collections::HashMap::new(),
            start: Instant::now(),
            viewer: None,
            viewer_img: Arc::new(Mutex::new(None)),
            viewer_zoom: 1.0,
            viewer_info_open: false,
            viewer_info: Arc::new(Mutex::new(None)),
            viewer_ext: Arc::new(Mutex::new(None)),
            viewer_confirm_del: false,
            viewer_desc: String::new(),
            viewer_date: String::new(),
            caption_loaded_for: None,
            switch_open: false,
            switch_list: Arc::new(Mutex::new(vec![])),
            selected: load_selected_identity(),
            hidden: read_hidden(),
            add_jar: String::new(),
            add_poll: None,
            pending_adds: vec![],
            cookie_import_path: String::new(),
            sizes: Arc::new(Mutex::new(std::collections::HashMap::new())),
            charged: Arc::new(Mutex::new(std::collections::HashSet::new())),
            hog_min_mb: String::new(),
            hog_stats: None,
            all_items: Arc::new(Mutex::new(vec![])),
            filter_bits: 0,
            scan_started: Arc::new(AtomicBool::new(false)),
            account_epoch: Arc::new(AtomicU64::new(0)),
            last_nav: Instant::now(),
            pending_deeplink: deeplink.clone(),
        };
        app.load_settings();
        // A link remembers who created it: pick that profile before the boot
        // capture below, so the target is opened under the right account.
        if let Some(email) = deeplink.as_ref().and_then(|(k, _)| k.account()) {
            if let Some(acc) = library::load_registry().into_iter().find(|a| a.email == email) {
                app.selected = Some((acc.jar.clone(), acc.email.clone()));
                set_setting("selected_jar", &acc.jar);
                set_setting("selected_email", &acc.email);
            }
        }
        // A bad link must say so; never leave a blank viewer behind it.
        if bad_link {
            let _ = app.tx.send(Msg::Toast {
                msg: "unsupported link — use gphotos://photo/<key>, gphotos://album/<key>, or a .gphotoslink file".into(),
                is_error: true,
            });
        }
        // on boot: open straight into the photos grid (optimistic); only fall
        // back to the sign-in page when there is genuinely no session. a real
        // session has oPEP7c = the account email; the sign-in shell does not.
        {
            let sess = app.lib_session.clone();
            let tx = app.tx.clone();
            let sel = app.selected.clone();
            let epoch_sh = app.account_epoch.clone();
            let epoch = epoch_sh.load(Ordering::Relaxed);
            thread::spawn(move || {
                let s = capture_selected(&sel);
                if epoch_sh.load(Ordering::Relaxed) != epoch {
                    return; // a switch happened before this capture landed
                }
                let ok = s.as_ref().map(|s| !s.email.is_empty()).unwrap_or(false);
                *sess.lock().unwrap() = s;
                let _ = tx.send(Msg::SessionReady(ok));
            });
        }
        // progressive thumb fill: keeps pulling the next 60 missing thumbs
        // every few seconds until the whole loaded view is cached, so
        // scrolling down never shows empty placeholders for long
        {
            let items = app.lib_items.clone();
            let thumbs = app.thumbs.clone();
            let sess = app.lib_session.clone();
            let tx = app.tx.clone();
            let filling = app.thumb_filling.clone();
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(5));
                let Some(s) = sess.lock().unwrap().clone() else { continue };
                let jobs: Vec<(String, String)> = {
                    let th = thumbs.lock().unwrap();
                    // filter before cloning: on a large library the old order
                    // allocated ~12k throwaway Strings every 5 seconds
                    items
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|i| !i.thumb.is_empty() && !th.contains_key(&i.media_key))
                        .take(120)
                        .map(|i| (i.media_key.clone(), i.thumb.clone()))
                        .collect()
                };
                if jobs.is_empty() || filling.swap(true, Ordering::Relaxed) {
                    continue;
                }
                fetch_thumbs_for(&s, &jobs, &tx);
                filling.store(false, Ordering::Relaxed);
            });
        }
        app
    }

    fn refresh_accounts(&self) {
        let accounts = self.accounts.clone();
        thread::spawn(move || {
            let (ok, out) = run_cmd(Command::new(gotohp_bin()).arg("creds").arg("list"));
            let list: Vec<String> = if ok {
                out.lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty() && !l.to_lowercase().contains("no credentials"))
                    .collect()
            } else {
                vec![]
            };
            if let Ok(mut g) = accounts.lock() {
                *g = list;
            }
        });
    }

    fn spawn_action<F: FnOnce(mpsc::Sender<Msg>) + Send + 'static>(&self, f: F) {
        if self.busy.load(Ordering::Relaxed) {
            return;
        }
        self.busy.store(true, Ordering::Relaxed);
        let dirty = self.snap_dirty.clone();
        let busy = self.busy.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            f(tx.clone());
            dirty.store(true, Ordering::Relaxed);
            busy.store(false, Ordering::Relaxed);
        });
    }

    fn watcher_action(&self, action: &'static str) {
        self.spawn_action(move |tx| {
            let send = |tx: &mpsc::Sender<Msg>, ok: bool, msg: String| {
                let _ = tx.send(Msg::Toast { msg, is_error: !ok });
            };
            match action {
                "start" => {
                    if !unit_installed() {
                        let (ok, msg) = run_cmd(
                            Command::new("bash")
                                .arg(format!("{}/google-unlimited.sh", app_root().display()))
                                .arg("enable"),
                        );
                        if !ok {
                            send(&tx, false, format!("{msg} — sign in first?"));
                            return;
                        }
                    }
                    let (ok, msg) = run_cmd(
                        Command::new("systemctl").arg("--user").arg("start").arg(UNIT),
                    );
                    send(
                        &tx,
                        ok,
                        if ok { "watcher started".into() } else { msg },
                    );
                }
                "stop" => {
                    let (ok, msg) = run_cmd(
                        Command::new("systemctl").arg("--user").arg("stop").arg(UNIT),
                    );
                    send(&tx, ok, if ok { "watcher stopped".into() } else { msg });
                }
                "autostart_on" | "autostart_off" => {
                    let flag = if action.ends_with("on") { "enable" } else { "disable" };
                    let (ok, msg) = run_cmd(
                        Command::new("bash")
                            .arg(format!("{}/google-unlimited.sh", app_root().display()))
                            .arg(flag),
                    );
                    send(
                        &tx,
                        ok,
                        if ok {
                            format!("autostart {flag}d").replace("enabledd", "enabled")
                        } else {
                            msg
                        },
                    );
                }
                _ => {}
            }
        });
    }

    fn rescan(&self) {
        self.spawn_action(move |tx| {
            let dirs = read_dirs();
            let mut cmd = Command::new("bash");
            cmd.arg(format!("{}/google-unlimited.sh", app_root().display()))
                .arg("watch")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .stdin(std::process::Stdio::null());
            for d in &dirs {
                cmd.arg(d);
            }
            match cmd.spawn() {
                Ok(_) => {
                    let _ = tx.send(Msg::Toast {
                        msg: "rescan started in background".into(),
                        is_error: false,
                    });
                }
                Err(e) => {
                    let _ = tx.send(Msg::Toast { msg: e.to_string(), is_error: true });
                }
            }
        });
    }

    fn save_dirs(&self, text: String) {
        self.spawn_action(move |tx| {
            let path = app_root().join("watch_dirs.txt");
            let lines: Vec<&str> = text
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect();
            let saved = if lines.is_empty() {
                match fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e),
                }
            } else {
                write_secret_file(&path, &(lines.join("\n") + "\n"))
            };
            let (msg, is_error) = match saved {
                Ok(()) => ("folders saved".to_string(), false),
                Err(e) => (format!("couldn't save folders: {e}"), true),
            };
            let _ = tx.send(Msg::Toast { msg, is_error });
        });
    }

    // Add / Create open the chosen profile's Google page and watch discovery,
    // never asking for a token. Detected identities are staged until Add.
    fn add_account(&mut self, signup: bool) {
        if self.add_poll.is_some() {
            return;
        }
        let jar = if self.add_jar.is_empty() {
            self.selected
                .as_ref()
                .map(|(j, _)| j.clone())
                .unwrap_or_default()
        } else {
            self.add_jar.clone()
        };
        if jar.is_empty() {
            self.pending_toasts.push((
                "no browser profile found yet — rescan first".into(),
                true,
                Instant::now(),
            ));
            return;
        }
        let url = if signup {
            "https://accounts.google.com/signup/v2/webcreateaccount?flowName=GlifWebSignIn&flowEntry=SignUp"
        } else {
            "https://accounts.google.com/AddSession"
        };
        open_jar_url(&jar, url);
        let poll_jar = jar.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.add_poll = Some(AddPoll {
            cancel: cancel.clone(),
            jar,
        });
        let tx = self.tx.clone();
        let epoch_sh = self.account_epoch.clone();
        let epoch = epoch_sh.load(Ordering::Relaxed);
        thread::spawn(move || {
            // empty batch is the UI's signal that detection ended (cancel,
            // account switch or timeout) so "watching…" cannot go stale
            let stop = || {
                let _ = tx.send(Msg::PendingAccounts(vec![]));
            };
            let known: HashSet<AccountKey> = library::load_registry()
                .into_iter()
                .map(|r| (r.jar, r.email))
                .collect();
            let mut seen: HashSet<AccountKey> = HashSet::new();
            for _ in 0..60 {
                if cancel.load(Ordering::Relaxed) || epoch_sh.load(Ordering::Relaxed) != epoch {
                    stop();
                    return;
                }
                thread::sleep(Duration::from_secs(3));
                if cancel.load(Ordering::Relaxed) || epoch_sh.load(Ordering::Relaxed) != epoch {
                    stop();
                    return;
                }
                let fresh: Vec<_> = library::discover_accounts()
                    .into_iter()
                    .filter(|r| r.jar == poll_jar)
                    .filter(|r| !known.contains(&(r.jar.clone(), r.email.clone())))
                    .filter(|r| seen.insert((r.jar.clone(), r.email.clone())))
                    .collect();
                if !fresh.is_empty() {
                    let _ = tx.send(Msg::PendingAccounts(fresh));
                    return;
                }
            }
            stop();
            let _ = tx.send(Msg::Toast {
                msg: "no new sign-in detected after 3 minutes — rescan, or use the labelled token backup".into(),
                is_error: true,
            });
        });
    }

    // open the sign-in page in the user's own browser, then keep reading the
    // local cookie stores until the oauth_token cookie appears — no DevTools
    fn signin_auto(&self) {
        const URL: &str = "https://accounts.google.com/embedded/setup/v2/android";
        // Open the real browser profile discovery reads. Keep the child so we
        // can close it once sign-in is detected; on Windows/macOS the OS opener
        // may hand off to a running browser and return no child, which is fine.
        let jar = self
            .selected
            .as_ref()
            .map(|(j, _)| j.clone())
            .unwrap_or_default();
        let browser: Option<std::process::Child> = if let Some((exe, args)) = jar_browser_argv(&jar, URL) {
            Command::new(&exe).args(&args).spawn().ok()
        } else {
            spawn_open_url(URL)
        };
        let browser = Arc::new(Mutex::new(browser));

        let sel = self.selected.clone();
        let browser2 = browser.clone();
        self.spawn_action(move |tx| {
            // tell the user up front whether this machine even has a session
            let alive = capture_selected(&sel).is_some();
            let _ = tx.send(Msg::Toast {
                msg: if alive {
                    "browser opened — this machine has a live Google session, the page may only ask to pick an account".into()
                } else {
                    "browser opened — no live Google session on this machine, so you WILL need to type the password once".into()
                },
                is_error: false,
            });
            for _ in 0..100 {
                // every browser at once, via the cookie helper
                let mut tok = String::new();
                if let Some(out) = library::run_python_script(&app_root().join("gcookies.py")) {
                    for line in out.lines() {
                        if let Some(v) = line.strip_prefix("oauth_token=") {
                            tok = v.trim().to_string();
                            break;
                        }
                    }
                }
                if !tok.is_empty() {
                    set_setting("logged_out", "0");
                    let _ = tx.send(Msg::Toast {
                        msg: "token captured — connecting your account…".into(),
                        is_error: false,
                    });
                    let _ = tx.send(Msg::Token(tok));
                    let _ = tx.send(Msg::SignedIn);
                    let _ = tx.send(Msg::SignInDone);
                    // close only the browser process we launched for sign-in
                    if let Some(mut child) = browser2.lock().unwrap().take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    return;
                }
                std::thread::sleep(Duration::from_secs(3));
            }
            let _ = tx.send(Msg::Toast {
                msg: "gave up after 5 minutes of waiting — no oauth_token appeared; paste it manually below".into(),
                is_error: true,
            });
        });
    }

    // ---- Library: the Google-Photos-Toolkit port ---------------------------

    // install a locally-computed subset as the current grid (no network)
    fn show_subset(&self, view: &str, mut items: Vec<library::MediaItem>, _desc: bool) {
        // "< back" reads view_prev, which fetch_lib normally maintains — a
        // local subset skips fetch_lib, so remember the origin here too
        let cur = self.lib_view.lock().unwrap().clone();
        {
            let mut prev = self.view_prev.lock().unwrap();
            if cur != view {
                *prev = cur.clone();
            }
        }
        if cur != view {
            let snap = self.lib_items.lock().unwrap().clone();
            if !snap.is_empty() {
                self.view_snapshots.lock().unwrap().insert(cur, snap);
            }
        }
        // local subsets come from all_items, which is size-sorted — always
        // re-sort newest-first so the date groups read correctly
        items.sort_by(|a, b| b.ts.cmp(&a.ts));
        *self.lib_view.lock().unwrap() = view.to_string();
        self.lib_next.lock().unwrap().take();
        self.lib_selected.lock().unwrap().clear();
        *self.lib_items.lock().unwrap() = items.clone();
        self.items_gen.fetch_add(1, Ordering::Relaxed);
        // these items were never a fetched "page", so their thumbnails were
        // not queued — kick off the same cached fetch the normal grid uses
        if let Some(s) = self.lib_session.lock().unwrap().clone() {
            let jobs: Vec<(String, String)> = items
                .iter()
                .take(120)
                .map(|i| (i.media_key.clone(), i.thumb.clone()))
                .filter(|(_, u)| !u.is_empty())
                .collect();
            let tx = self.tx.clone();
            thread::spawn(move || fetch_thumbs_for(&s, &jobs, &tx));
        }
    }

    // structured local filter: keep items matching `keep` from the one-time
    // scan. Before that scan finishes all_items is empty, so filter the grid
    // that is already loaded instead — no RPC in either case.
    fn local_subset<F: Fn(&library::MediaItem) -> bool>(&self, view: &str, desc: bool, keep: F) {
        let items: Vec<library::MediaItem> = {
            let all = self.all_items.lock().unwrap();
            if all.is_empty() {
                self.lib_items
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|i| keep(i))
                    .cloned()
                    .collect()
            } else {
                all.iter().filter(|i| keep(i)).cloned().collect()
            }
        };
        self.show_subset(view, items, desc);
    }

    // republish the grid from the current bitmask. Same source fallback as
    // local_subset (all_items once the scan lands, else the loaded grid).
    fn apply_local_filters(&mut self) {
        let bits = self.filter_bits;
        // keep the search box in sync with the active combination; otherwise
        // "videos" from an earlier single chip lingers on a composite view
        self.lib_search = match bits {
            0 => String::new(),
            FILTER_VIDEOS => "videos".into(),
            FILTER_PHOTOS => "photos".into(),
            FILTER_LARGE => "large".into(),
            _ => filter_view_name(bits).trim_start_matches("filter:").replace('+', " + "),
        };
        if bits == 0 {
            self.fetch_lib("recent".into());
            return;
        }
        let sizes = if bits & FILTER_LARGE != 0 {
            self.sizes.lock().unwrap().clone()
        } else {
            std::collections::HashMap::new()
        };
        let items: Vec<library::MediaItem> = {
            let all = self.all_items.lock().unwrap();
            let keep = |i: &library::MediaItem| {
                filter_matches(bits, i.is_video, sizes.get(&i.media_key).copied().unwrap_or(0))
            };
            if all.is_empty() {
                self.lib_items.lock().unwrap().iter().filter(|i| keep(i)).cloned().collect()
            } else {
                all.iter().filter(|i| keep(i)).cloned().collect()
            }
        };
        self.show_subset(filter_view_name(bits), items, false);
    }

    // bursts / stacks: group items captured within 3 s of each other with the
    // same dimensions and media type. Uses the one-time scan when available,
    // else the grid that is already loaded (mirrors local_subset's fallback).
    fn show_bursts(&mut self) {
        let src: Vec<library::MediaItem> = {
            let all = self.all_items.lock().unwrap();
            if all.is_empty() {
                self.lib_items.lock().unwrap().clone()
            } else {
                all.clone()
            }
        };
        let mut sorted = src;
        sorted.retain(|i| i.ts > 0); // no timestamp => cannot be a burst
        sorted.sort_by_key(|i| i.ts);

        let mut groups: Vec<Vec<library::MediaItem>> = vec![];
        let mut cur: Vec<library::MediaItem> = vec![];
        for it in sorted {
            let new_group = cur
                .last()
                .map(|last| {
                    it.ts - last.ts > 3
                        || it.w != last.w
                        || it.h != last.h
                        || it.is_video != last.is_video
                })
                .unwrap_or(false);
            if new_group {
                if cur.len() >= 3 {
                    groups.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            }
            cur.push(it);
        }
        if cur.len() >= 3 {
            groups.push(cur);
        }

        let mut info = std::collections::HashMap::new();
        let mut items = Vec::new();
        for (idx, g) in groups.iter().enumerate() {
            let n = g.len();
            for it in g {
                info.insert(it.media_key.clone(), (idx, n));
                items.push(it.clone());
            }
        }
        self.burst_info = info;
        // show_subset re-sorts newest-first; groups stay contiguous because
        // their time ranges never overlap (a >3 s gap separates them).
        self.show_subset("bursts", items, false);
    }

    // "file:xxx" searches the filenames/captions we already measured locally;
    // everything else goes to Google's own search RPC
    fn run_search(&self, q: &str) {
        let q = q.trim();
        if let Some(rest) = q.strip_prefix("file:").or_else(|| q.strip_prefix("filename:")) {
            self.fetch_lib(format!("filename:{}", rest.trim()));
        } else {
            self.fetch_lib(format!("search:{}", q));
        }
    }

    fn fetch_lib(&self, view: String) {
        if self.lib_busy.swap(true, Ordering::Relaxed) {
            return;
        }
        {
            let mut prev = self.view_prev.lock().unwrap();
            let cur = self.lib_view.lock().unwrap().clone();
            if cur != view {
                *prev = cur;
            }
        }
        *self.lib_view.lock().unwrap() = view.clone();
        // listing views (albums / shared / links) show tiles, not photos, so
        // stale rows from the last view must not linger under them
        if matches!(view.as_str(), "albums" | "shared" | "links" | "dupes")
            || view.starts_with("filename:")
        {
            self.lib_items.lock().unwrap().clear();
            self.lib_next.lock().unwrap().take();
            // a failed listing fetch must not leave the previous view's tiles
            // on screen under a new title
            if matches!(view.as_str(), "albums" | "shared") {
                self.lib_albums.lock().unwrap().clear();
            }
            if view == "links" {
                self.lib_links.lock().unwrap().clear();
            }
            if view == "things" {
                self.lib_things.lock().unwrap().clear();
            }
        } else if view.starts_with("search:") {
            // keep the current grid: a failed search must not blank it. Only
            // drop the old page token so scrolling cannot mix two lists.
            self.lib_next.lock().unwrap().take();
        }
        let items_sh = self.lib_items.clone();
        let items_gen_sh = self.items_gen.clone();
        let albums_sh = self.lib_albums.clone();
        let links_sh = self.lib_links.clone();
        let things_sh = self.lib_things.clone();
        let sess_sh = self.lib_session.clone();
        let next_sh = self.lib_next.clone();
        let all_sh = self.all_items.clone();
        let bulk_sh = self.bulk.clone();
        let tx = self.tx.clone();
        let busy = self.lib_busy.clone();
        let sel = self.selected.clone();
        let epoch_sh = self.account_epoch.clone();
        let epoch = epoch_sh.load(Ordering::Relaxed);
        thread::spawn(move || {
            // covers the local-only filename:/dupes branches too: their results
            // come from the old account's scan snapshot, which a switch cleared
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                busy.store(false, Ordering::Relaxed);
                return;
            }
            let fail_msg = format!(
                "couldn't load {} — Google Photos didn't answer; showing your previous view",
                view
            );
            // filename:/caption:/dupes need no network: search the one-time scan
            if let Some(q) = view.strip_prefix("filename:") {
                let needle = q.to_lowercase();
                let bulk = bulk_sh.lock().unwrap();
                let mut hits: Vec<library::MediaItem> = all_sh
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|it| {
                        bulk.get(&it.media_key)
                            .map(|(f, d)| {
                                f.to_lowercase().contains(&needle)
                                    || d.to_lowercase().contains(&needle)
                            })
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                hits.sort_by(|a, b| b.ts.cmp(&a.ts));
                let _ = tx.send(Msg::Lib(hits, view.clone(), None));
                busy.store(false, Ordering::Relaxed);
                return;
            }
            if view == "dupes" {
                let bulk = bulk_sh.lock().unwrap();
                let mut groups: std::collections::HashMap<(u64, i64), Vec<String>> =
                    Default::default();
                // group by (filename, timestamp) using the scan data we kept
                for it in all_sh.lock().unwrap().iter() {
                    if let Some((name, _)) = bulk.get(&it.media_key) {
                        if !name.is_empty() {
                            groups
                                .entry((crc32(name.as_bytes()), it.ts))
                                .or_default()
                                .push(it.media_key.clone());
                        }
                    }
                }
                let keys: std::collections::HashSet<String> = groups
                    .into_values()
                    .filter(|v| v.len() > 1)
                    .flatten()
                    .collect();
                let all = all_sh.lock().unwrap();
                let mut hits: Vec<library::MediaItem> = all
                    .iter()
                    .filter(|it| keys.contains(&it.media_key))
                    .cloned()
                    .collect();
                hits.sort_by(|a, b| b.ts.cmp(&a.ts));
                let _ = tx.send(Msg::Lib(hits, view.clone(), None));
                busy.store(false, Ordering::Relaxed);
                return;
            }
            // reuse the session between calls; re-capture only when it dies or
            // it belongs to a different identity than the one we are loading
            let sess = {
                let guard = sess_sh.lock().unwrap();
                guard.clone().filter(|s| session_matches(&sel, s))
            };
            let s = match sess {
                Some(s) => Some(s),
                None => {
                    let s = capture_selected(&sel);
                    // a switch may have landed while the capture ran: never
                    // publish the old identity into the new account's cache
                    if epoch_sh.load(Ordering::Relaxed) != epoch {
                        busy.store(false, Ordering::Relaxed);
                        return;
                    }
                    if s.is_some() {
                        *sess_sh.lock().unwrap() = s.clone();
                    }
                    s
                }
            };
            let Some(s) = s else {
                busy.store(false, Ordering::Relaxed);
                let _ = tx.send(Msg::Toast {
                    msg: "couldn't get your photos.google.com session — sign into it in Firefox/Chrome on this machine, then try again".into(),
                    is_error: true,
                });
                return;
            };
            // last line of defence: never RPC with an identity other than the
            // one this grid is showing, even if a stale cache survived a switch
            if !session_matches(&sel, &s) {
                busy.store(false, Ordering::Relaxed);
                return;
            }
            // every network branch below writes shared state, so a switch that
            // happened before/during this fetch must abandon the whole worker
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                busy.store(false, Ordering::Relaxed);
                return;
            }
            let fetch_page = |s: &library::Session| -> (Vec<library::MediaItem>, Option<String>) {
                match view.as_str() {
                    "recent" => library::recent(s, None),
                    "shared" => (vec![], None),
                    "favorites" => library::favorites(s, None),
                    "archive" => library::archive(s, None),
                    "trash" => library::trash(s, None),
                    "locked" => library::locked(s, None),
                    q if q.starts_with("search:") => library::search(s, &q[7..], None),
                    a if a.starts_with("album:") => library::album_items(s, &a[6..], None),
                    l if l.starts_with("link:") => library::album_items(s, &l[5..], None),
                    _ => (vec![], None),
                }
            };
            let (mut items, mut _next) = fetch_page(&s);
            // google sometimes answers empty on one call; one gentle retry
            if items.is_empty()
                && (matches!(
                    view.as_str(),
                    "recent" | "favorites" | "archive" | "trash" | "locked"
                ) || view.starts_with("search:"))
            {
                thread::sleep(Duration::from_secs(3));
                let (it2, nx2) = fetch_page(&s);
                items = it2;
                _next = nx2;
            }
            // a failed rpc returned an empty vec — never mistake it for a
            // genuinely empty view and blank the grid. Skip the Msg::Lib send
            // so the current grid and lib_next stay exactly as they were.
            if library::last_rpc_failed() {
                busy.store(false, Ordering::Relaxed);
                let _ = tx.send(Msg::Toast { msg: fail_msg.clone(), is_error: true });
                return;
            }
            // album / shared-album views are tile listings, not photo pages
            if view == "albums" || view == "shared" {
                let (mut albums, _) = library::albums(&s, None);
                if library::last_rpc_failed() {
                    busy.store(false, Ordering::Relaxed);
                    let _ = tx.send(Msg::Toast { msg: fail_msg.clone(), is_error: true });
                    return;
                }
                if view == "shared" {
                    albums.retain(|a| a.is_shared);
                }
                if epoch_sh.load(Ordering::Relaxed) != epoch {
                    busy.store(false, Ordering::Relaxed);
                    return;
                }
                *albums_sh.lock().unwrap() = albums.clone();
                let _ = tx.send(Msg::Albums(albums.clone(), view.clone()));
                // covers for the tiles
                let jobs: Vec<(String, String)> = albums
                    .iter()
                    .map(|a| (format!("album:{}", a.key), a.thumb.clone()))
                    .filter(|(_, u)| !u.is_empty())
                    .collect();
                fetch_thumbs_for(&s, &jobs, &tx);
                busy.store(false, Ordering::Relaxed);
                return;
            }
            if view == "links" {
                let (links, _) = library::links(&s, None);
                if library::last_rpc_failed() {
                    busy.store(false, Ordering::Relaxed);
                    let _ = tx.send(Msg::Toast { msg: fail_msg.clone(), is_error: true });
                    return;
                }
                if epoch_sh.load(Ordering::Relaxed) != epoch {
                    busy.store(false, Ordering::Relaxed);
                    return;
                }
                *links_sh.lock().unwrap() = links.clone();
                let _ = tx.send(Msg::Links(links, view.clone()));
                busy.store(false, Ordering::Relaxed);
                return;
            }
            if view == "things" {
                let things = library::things(&s);
                if library::last_rpc_failed() {
                    busy.store(false, Ordering::Relaxed);
                    let _ = tx.send(Msg::Toast { msg: fail_msg.clone(), is_error: true });
                    return;
                }
                if epoch_sh.load(Ordering::Relaxed) != epoch {
                    busy.store(false, Ordering::Relaxed);
                    return;
                }
                *things_sh.lock().unwrap() = things.clone();
                let _ = tx.send(Msg::Things(things, view.clone()));
                busy.store(false, Ordering::Relaxed);
                return;
            }
            items.sort_by(|a, b| b.ts.cmp(&a.ts));
            // a switch while these pages were in flight must not inject the old
            // account's library into the new account's grid
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                busy.store(false, Ordering::Relaxed);
                return;
            }
            // the matching Msg::Lib handler writes lib_items/lib_next/items_gen
            // only if the view still matches — a late reply must not clobber a
            // local subset the user switched to meanwhile
            let _ = tx.send(Msg::Lib(items.clone(), view.clone(), _next));
            // thumbnails: throttled + disk-cached
            let jobs: Vec<(String, String)> = items
                .iter()
                .take(120)
                .map(|i| (i.media_key.clone(), i.thumb.clone()))
                .filter(|(_, u)| !u.is_empty())
                .collect();
            fetch_thumbs_for(&s, &jobs, &tx);
            busy.store(false, Ordering::Relaxed);
        });
    }

    fn fetch_more(&self) {
        if self.lib_busy.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(next) = self.lib_next.lock().unwrap().clone() else {
            self.lib_busy.store(false, Ordering::Relaxed);
            return;
        };
        let view = self.current_lib_view();
        let sess_sh = self.lib_session.clone();
        let next_sh = self.lib_next.clone();
        let tx = self.tx.clone();
        let busy = self.lib_busy.clone();
        let sel = self.selected.clone();
        let epoch_sh = self.account_epoch.clone();
        let epoch = epoch_sh.load(Ordering::Relaxed);
        thread::spawn(move || {
            let s = match sess_sh.lock().unwrap().clone() {
                Some(s) if session_matches(&sel, &s) => s,
                _ => {
                    busy.store(false, Ordering::Relaxed);
                    return;
                }
            };
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                busy.store(false, Ordering::Relaxed);
                return;
            }
            let page = Some(next.as_str());
            let (items, nxt) = if view == "recent" {
                library::recent(&s, page)
            } else if view == "favorites" {
                library::favorites(&s, page)
            } else if view == "archive" {
                library::archive(&s, page)
            } else if view == "trash" {
                library::trash(&s, page)
            } else if view == "locked" {
                library::locked(&s, page)
            } else if let Some(q) = view.strip_prefix("search:") {
                library::search(&s, q, page)
            } else if let Some(a) = view.strip_prefix("album:") {
                library::album_items(&s, a, page)
            } else if let Some(l) = view.strip_prefix("link:") {
                library::album_items(&s, l, page)
            } else {
                (vec![], None)
            };
            // a failed page must not advance (or clear) lib_next — leave the
            // token as it is so the user can retry the same page
            if library::last_rpc_failed() {
                busy.store(false, Ordering::Relaxed);
                let _ = tx.send(Msg::Toast {
                    msg: format!(
                        "couldn't load more {} — Google Photos didn't answer; try again",
                        view
                    ),
                    is_error: true,
                });
                return;
            }
            // the view string is identical across accounts, so only the epoch
            // can stop an old account's page appending to the new account's grid
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                busy.store(false, Ordering::Relaxed);
                return;
            }
            let _ = tx.send(Msg::MoreLib(items.clone(), view.clone(), nxt));
            let jobs: Vec<(String, String)> = items
                .iter()
                .take(60)
                .map(|i| (i.media_key.clone(), i.thumb.clone()))
                .filter(|(_, u)| !u.is_empty())
                .collect();
            fetch_thumbs_for(&s, &jobs, &tx);
            busy.store(false, Ordering::Relaxed);
        });
    }

    // context-menu actions must affect exactly the clicked photo, never a
    // selection the user happened to have when they right-clicked
    fn act_on_one(&self, key: &str, action: &str) {
        self.lib_action_with(action, vec![key.to_string()]);
    }

    fn lib_action(&self, action: &str) {
        let sel: Vec<String> = self.lib_selected.lock().unwrap().iter().cloned().collect();
        self.lib_action_with(action, sel);
    }

    fn lib_action_with(&self, action: &str, sel: Vec<String>) {
        if sel.is_empty() {
            return;
        }
        let action = action.to_string();
        let sess_sh = self.lib_session.clone();
        let items_sh = self.lib_items.clone();
        let sel_sh = self.lib_selected.clone();
        let bulk_sh = self.bulk.clone();
        let view = self.current_lib_view();
        let album_name = self.album_name.clone();
        let album_target = self.album_target.clone();
        let copy_links = self.copy_links;
        self.spawn_action(move |tx| {
            let Some(s) = sess_sh.lock().unwrap().clone() else {
                let _ = tx.send(Msg::Toast { msg: "library session lost — refresh first".into(), is_error: true });
                return;
            };
            let items = items_sh.lock().unwrap().clone();
            let dedup: Vec<String> = sel
                .iter()
                .filter_map(|k| items.iter().find(|i| i.media_key == *k).map(|i| i.dedup_key.clone()))
                .collect();
            let media: Vec<String> = sel.clone();
            let pairs: Vec<(String, String)> = media
                .iter()
                .cloned()
                .zip(dedup.iter().cloned())
                .collect();
            let ok = match action.as_str() {
                "download" => {
                    // cap the batch: thousands of originals would hammer the
                    // CDN and fill the disk. The user can select a smaller set.
                    const MAX_DOWNLOAD: usize = 200;
                    let dir = dirs::download_dir()
                        .unwrap_or_else(std::env::temp_dir)
                        .join("gphotos-export");
                    let _ = std::fs::create_dir_all(&dir);
                    if sel.len() > MAX_DOWNLOAD {
                        tx.send(Msg::Toast {
                            msg: format!(
                                "downloading the first {} of {} selected — narrow the selection for the rest",
                                MAX_DOWNLOAD, sel.len()
                            ),
                            is_error: false,
                        }).ok();
                    }
                    let chosen: Vec<&library::MediaItem> = items
                        .iter()
                        .filter(|i| media.contains(&i.media_key))
                        .take(MAX_DOWNLOAD)
                        .collect();
                    let bulk = bulk_sh.lock().unwrap().clone();
                    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
                    let mut done = 0usize;
                    let mut failed = 0usize;
                    for it in chosen {
                        let suffix = if it.is_video { "=dv" } else { "=d" };
                        // prefer the real filename from the one-time scan; keep
                        // it safe for every filesystem and avoid overwrites
                        let raw = bulk
                            .get(&it.media_key)
                            .map(|(f, _)| f.as_str())
                            .unwrap_or(it.media_key.as_str());
                        let unique = export_filename(raw, &it.media_key, it.is_video, &mut used);
                        let dest = dir.join(unique);
                        let got = if dest.exists() {
                            Some(std::fs::read(&dest).unwrap_or_default())
                        } else {
                            fetch_media(&it.thumb, &library::fife_cookie_header(), suffix)
                        };
                        if let Some(bytes) = got.filter(|b| !b.is_empty()) {
                            if std::fs::write(&dest, bytes).is_ok() {
                                done += 1;
                                continue;
                            }
                        }
                        failed += 1;
                    }
                    tx.send(Msg::Toast {
                        msg: format!(
                            "saved {} originals to {} ({} failed, max {})",
                            done,
                            dir.display(),
                            failed,
                            MAX_DOWNLOAD
                        ),
                        is_error: done == 0,
                    })
                    .ok();
                    true
                }
                "share" => match library::create_share_link(&s, &media) {
                    Some(url) => {
                        let copied = copy_links && copy_text_to_clipboard(&url);
                        tx.send(Msg::Toast {
                            msg: format!("{}{}", if copied { "link copied: " } else { "link: " }, url),
                            is_error: false,
                        })
                        .ok();
                        true
                    }
                    None => false,
                },
                "trash" => library::set_trashed(&s, &dedup, true),
                "restore" => library::set_trashed(&s, &dedup, false),
                "archive" => library::set_archived(&s, &dedup, true),
                "unarchive" => library::set_archived(&s, &dedup, false),
                "fav" => library::set_favorite(&s, &dedup, true),
                "unfav" => library::set_favorite(&s, &dedup, false),
                "lock" => library::lock_items(&s, &dedup),
                "unlock" => library::unlock_items(&s, &dedup),
                "rmalbum" => library::remove_from_album(&s, &media),
                "toalbum" if !album_name.trim().is_empty() => {
                    library::add_to_album(&s, &media, None, Some(album_name.trim()))
                }
                "toalbum_existing" => match &album_target {
                    Some(k) => library::add_to_album(&s, &media, Some(k), None),
                    None => false,
                },
                "savelink" => library::save_shared_to_library(&s, &view[5..], &media),
                "copydesc" => {
                    let n = library::copy_exif_description(&s, &pairs);
                    let _ = tx.send(Msg::Toast { msg: format!("captions copied: {n}"), is_error: false });
                    true
                }
                "datefromname" => {
                    let n = library::set_timestamp_from_filename(&s, &pairs);
                    let _ = tx.send(Msg::Toast { msg: format!("dates fixed from filename: {n}"), is_error: false });
                    true
                }
                "exportmeta" => match library::export_metadata(&s, &media) {
                    Some(p) => {
                        let _ = tx.send(Msg::Toast { msg: format!("metadata CSV → {p}"), is_error: false });
                        true
                    }
                    None => {
                        let _ = tx.send(Msg::Toast { msg: "metadata export failed".into(), is_error: true });
                        false
                    }
                },
                "delforever" => {
                    library::set_trashed(&s, &dedup, true) && library::delete_forever(&s, &dedup)
                }
                _ => false,
            };
            let inverse = match action.as_str() {
                "trash" => Some("restore"),
                "archive" => Some("unarchive"),
                "fav" => Some("unfav"),
                "unfav" => Some("fav"),
                "lock" => Some("unlock"),
                "unlock" => Some("lock"),
                _ => None,
            };
            if ok {
                if let Some(inv) = inverse {
                    let _ = tx.send(Msg::Undo(inv.into(), dedup.clone()));
                }
            }
            // the download arm reports its own totals and does not change the
            // library, so a generic success toast + refresh would be noise
            if action != "download" {
                let _ = tx.send(Msg::Toast {
                    msg: if ok {
                        format!("done: {} item(s) — refreshing…", sel.len())
                    } else {
                        "the operation didn't go through — try again".into()
                    },
                    is_error: !ok,
                });
                if ok {
                    sel_sh.lock().unwrap().retain(|k| !sel.contains(k));
                    // the rpc has landed: refresh now, not on a separate timer
                    // racing the write
                    let _ = tx.send(Msg::RefreshLib);
                }
            }
        });
    }

    fn fetch_quota(&self) {
        let sess_sh = self.lib_session.clone();
        let quota_sh = self.quota_txt.clone();
        let sel = self.selected.clone();
        let epoch_sh = self.account_epoch.clone();
        let epoch = epoch_sh.load(Ordering::Relaxed);
        self.spawn_action(move |tx| {
            let Some(s) = sess_sh.lock().unwrap().clone().or_else(|| capture_selected(&sel)) else {
                let _ = tx.send(Msg::Toast { msg: "no photos.google.com session — sign in first".into(), is_error: true });
                return;
            };
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                return; // old account's quota must not land after a switch
            }
            *sess_sh.lock().unwrap() = Some(s.clone());
            // photos are a small slice of an account that also holds drive and
            // mail, so the line leads with the slice this app can change
            let txt = match library::quota(&s) {
                Some(q) => {
                    let photo = human_gb(q.gphotos);
                    if q.available > 0 {
                        format!(
                            "photos {} · account {} of {} used",
                            photo,
                            human_gb(q.used),
                            human_gb(q.available)
                        )
                    } else {
                        format!("photos {} · account {} used", photo, human_gb(q.used))
                    }
                }
                None => "could not read storage quota".into(),
            };
            let failed = txt.contains("could not read");
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                return; // the quota RPC may have taken long enough for a switch
            }
            *quota_sh.lock().unwrap() = txt.clone();
            let _ = tx.send(Msg::Toast {
                msg: if failed { txt } else { "storage usage refreshed".into() },
                is_error: failed,
            });
        });
    }

    fn current_lib_view(&self) -> String {
        self.lib_view.lock().unwrap().clone()
    }

    // shared by the sidebar button, the selection strip and ctrl+a: in hogs
    // view the grid hides rows below the size filter, so filter here too
    fn select_all(&self) {
        let all: std::collections::HashSet<String> = if self.current_lib_view() == "hogs" {
            let min_bytes =
                (self.hog_min_mb.trim().parse::<f64>().unwrap_or(0.0) * 1_048_576.0) as u64;
            let sz = self.sizes.lock().unwrap();
            self.lib_items
                .lock()
                .unwrap()
                .iter()
                .filter(|i| *sz.get(&i.media_key).unwrap_or(&0) >= min_bytes)
                .map(|i| i.media_key.clone())
                .collect()
        } else {
            self.lib_items
                .lock()
                .unwrap()
                .iter()
                .map(|i| i.media_key.clone())
                .collect()
        };
        *self.lib_selected.lock().unwrap() = all;
    }

    fn add_creds(&self, token: String) {
        self.spawn_action(move |tx| {
            use std::io::Write;
            use std::process::Stdio;
            let mut child = match Command::new(gotohp_bin())
                .arg("creds")
                .arg("add")
                .arg("-")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Msg::Toast { msg: e.to_string(), is_error: true });
                    return;
                }
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(token.trim().as_bytes());
                let _ = stdin.write_all(b"\n");
            }
            let out = child.wait_with_output();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(
                    app_root().join("gotohp.config"),
                    fs::Permissions::from_mode(0o600),
                );
            }
            let (ok, msg) = match out {
                Ok(o) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                    (
                        o.status.success() && !text.to_lowercase().contains("error"),
                        text,
                    )
                }
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(Msg::Toast {
                msg: if ok {
                    "connected!".into()
                } else {
                    msg
                },
                is_error: !ok,
            });
        });
    }

    fn update_app(&self) {
        self.spawn_action(move |tx| {
            use std::io::Write;
            use std::process::Stdio;
            let mut child = match Command::new("bash")
                .arg(format!("{}/google-unlimited.sh", app_root().display()))
                .arg("update")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Msg::Toast { msg: e.to_string(), is_error: true });
                    return;
                }
            };
            // auto-answer any y/n prompt so it never hangs the UI
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"y\n");
            }
            let (ok, msg) = match child.wait_with_output() {
                Ok(o) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                    (o.status.success(), text)
                }
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(Msg::Toast {
                msg: if ok { format!("update checked: {}", msg.lines().last().unwrap_or("done")) } else { msg },
                is_error: !ok,
            });
        });
    }

    fn update_app_idle(&self) {}

    fn pick_folder(&self) {
        self.spawn_action(move |tx| {
            if let Some(path) = picked_folder(
                rfd::FileDialog::new()
                    .set_title("Choose a folder to sync")
                    .pick_folder(),
            ) {
                let _ = tx.send(Msg::PickedDirs(vec![path]));
            }
        });
    }
}

fn set_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let patrick = include_bytes!("PatrickHand.ttf").to_vec();
    let caveat = include_bytes!("Caveat-Bold.ttf").to_vec();
    fonts.font_data.insert(
        "patrick".into(),
        egui::FontData::from_owned(patrick).into(),
    );
    fonts.font_data.insert("caveat".into(), egui::FontData::from_owned(caveat).into());
    // handwriting everywhere
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .clear();
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push("patrick".into());
    // headings use Caveat by default size trick: same family list is fine
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .clear();
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("patrick".into());
    ctx.set_fonts(fonts);
}

fn set_theme(ctx: &egui::Context) {
    // handwriting at default sizes runs small; zoom the whole page up
    ctx.set_pixels_per_point(ctx.pixels_per_point() * 1.18);
    // start from the LIGHT theme so no widget inherits dark-theme white text
    let mut style = egui::Style::default();
    style.visuals = egui::Visuals::light();
    let v = &mut style.visuals;
    v.panel_fill = PAPER;
    v.window_fill = PAPER;
    v.extreme_bg_color = PAPER_DARK;
    v.override_text_color = Some(INK);
    v.widgets.noninteractive.bg_fill = PAPER;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.5, INK_SOFT);
    for state in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
    ] {
        state.bg_fill = Color32::from_rgb(0xff, 0xfd, 0xf5);
        state.weak_bg_fill = Color32::from_rgb(0xff, 0xfd, 0xf5);
        state.bg_stroke = Stroke::new(2.0, INK);
        state.fg_stroke = Stroke::new(2.0, INK);
        state.rounding = egui::Rounding::same(9.0);
    }
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(0xff, 0xf3, 0xd8);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0xff, 0xf3, 0xd8);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(0xff, 0xe9, 0xc9);
    style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(0xff, 0xe9, 0xc9);
    // highlighter-pen selection: yellow marker with ink text on top
    style.visuals.selection.bg_fill = Color32::from_rgb(0xff, 0xe9, 0xa8);
    style.visuals.selection.stroke = Stroke::new(2.0, INK);
    style.visuals.hyperlink_color = BLUE;
    // disabled controls must stay readable too
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(2.0, INK_SOFT);
    // sync every remaining default color into the sketch palette
    style.visuals.window_stroke = Stroke::new(2.0, INK);
    style.visuals.window_shadow = egui::Shadow::NONE;
    style.visuals.popup_shadow = egui::Shadow::NONE;
    style.visuals.striped = false;
    // the native scrollbar, in the same paper-and-ink palette (not a dark bar)
    style.visuals.extreme_bg_color = Color32::from_rgb(0xf4, 0xee, 0xdf);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(0xc9, 0xbe, 0xa6);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(0xbf, 0xb0, 0x8e);
    style.visuals.widgets.active.bg_fill = YELLOW;
    style.spacing.scroll = egui::style::ScrollStyle {
        foreground_color: false,
        ..egui::style::ScrollStyle::thin()
    };
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    ctx.set_style(style);
}

fn sketchy_heading(ui: &mut egui::Ui, text: &str) {
    let resp = ui.label(egui::RichText::new(text).size(19.0).strong());
    let r = resp.rect;
    let t = ui.input(|i| i.time as f32);
    sketchy_line_wavy(
        ui.painter(),
        Pos2::new(r.left() + 1.0, r.bottom() + 1.0),
        Pos2::new(r.left() + r.width() * 0.72, r.bottom() + 2.5),
        RED,
        2.0,
        text.len() as u32 * 31 + 7,
        t,
    );
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

// one month of the timeline, as the right-edge rail shows it: the items it holds
// and the first of them, which is where a click sends the grid
#[derive(Clone)]
struct MonthBucket {
    year: i64,
    month: u32,
    count: usize,
    first: String,
    ts: i64,
}

fn month_name(m: u32) -> &'static str {
    const M: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    M[((m.max(1) - 1) % 12) as usize]
}

// "2019", "jul 2019", "july 2019", "15 jul 2019", "2019-07", "2019-07-15"
fn parse_date_query(s: &str) -> Option<i64> {
    let t = s.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    // iso first: 2019-07, 2019/07/15
    let iso: Vec<&str> = t
        .split(|c: char| c == '-' || c == '/' || c == '.')
        .filter(|p| !p.is_empty())
        .collect();
    if iso.len() >= 2 && iso[0].len() == 4 {
        if let (Ok(y), Ok(m)) = (iso[0].parse::<i64>(), iso[1].parse::<u32>()) {
            if (1..=12).contains(&m) && y > 1800 {
                let d = iso
                    .get(2)
                    .and_then(|x| x.parse::<u32>().ok())
                    .filter(|d| (1..=31).contains(d))
                    .unwrap_or(1);
                return Some(library::days_from_civil(y, m, d) * 86400);
            }
        }
    }
    const NAMES: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let mut year = None;
    let mut month = None;
    let mut day = None;
    for word in t.split(|c: char| c.is_whitespace() || c == ',') {
        if word.is_empty() {
            continue;
        }
        if let Ok(n) = word.parse::<i64>() {
            if (1000..=9999).contains(&n) {
                year = Some(n);
            } else if (1..=31).contains(&n) {
                day = Some(n as u32);
            }
        } else if let Some(i) = NAMES.iter().position(|m| word.starts_with(m)) {
            month = Some(i as u32 + 1);
        }
    }
    let y = year?;
    Some(
        library::days_from_civil(y, month.unwrap_or(1), day.unwrap_or(1))
            .checked_mul(86400)?,
    )
}

// a still of the video at one second, for the seek bar's hover bubble; grabbed
// on demand only, one at a time, exactly like thumbfast does it for mpv
struct SeekThumb {
    key: String,
    at: f64,
    tex: Option<egui::TextureHandle>,
    slot: Arc<Mutex<Option<egui::ColorImage>>>,
    busy: Arc<AtomicBool>,
}

// A Colab notebook that walks tasks.tsv, downloads each original, keeps its
// date, re-uploads it through the free endpoint and checkpoints every file.
fn colab_notebook(count: usize) -> String {
    let py = r####"
# 1 — setup
!pip -q install requests
from google.colab import drive
drive.mount('/content/drive')
JOB = '/content/drive/MyDrive/gphotos-job'   # the folder you uploaded
!chmod +x {JOB}/gotohp-cli_amd64
!ls -la {JOB}
"####
    .replace("{JOB}", "$JOB");

    let run = r####"
# 3 — the run itself: resumable, one item at a time
import csv, json, os, re, subprocess, time, requests

COOKIE = "PASTE THE OUTPUT OF fifecookies.py HERE"
JOB = '/content/drive/MyDrive/gphotos-job'
WORK = '/content/work'
STATE = os.path.join(JOB, 'state.json')      # survives a disconnect
RESULTS = os.path.join(JOB, 'results.tsv')
os.makedirs(WORK, exist_ok=True)

state = json.load(open(STATE)) if os.path.exists(STATE) else {}
tasks = list(csv.DictReader(open(os.path.join(JOB, 'tasks.tsv')), delimiter='	'))
# failed items stay retryable: only completed keys are skipped
todo = [t for t in tasks if state.get(t['media_key']) not in ('ok',)]
print(f'{len(tasks)} in the job, {len(todo)} left to do')
if not os.path.exists(RESULTS):
    open(RESULTS, 'w').write('media_key	dedup	new_key	status
')

def save():
    json.dump(state, open(STATE, 'w'))

for n, t in enumerate(todo, 1):
    key, ts = t['media_key'], int(t['ts'])
    name = re.sub(r'[^\w.\- ]', '_', t['filename']) or key
    path = os.path.join(WORK, name)
    ok = False
    for attempt in range(4):                 # backoff, like rclone's retries
        try:
            r = requests.get(url_map[key], headers={'Cookie': COOKIE}, timeout=120)
            r.raise_for_status()
            open(path, 'wb').write(r.content)
            os.utime(path, (ts, ts))         # the free endpoint keeps this date
            out = subprocess.run(
                [f'{JOB}/gotohp-cli_amd64', 'upload', path, '--no-tui', '-l', 'warn'],
                capture_output=True, text=True, timeout=900)
            m = re.search(r'"mediaKey": "([^"]+)"', out.stdout + out.stderr)
            if out.returncode == 0 and m:
                with open(RESULTS, 'a') as f:
                    f.write(f"{key}	{t['dedup']}	{m.group(1)}	ok
")
                state[key] = m.group(1)
                ok = True
                break
        except Exception as e:
            print(f'  {name}: attempt {attempt + 1} failed: {e}')
            time.sleep(4 * (attempt + 1))
    if not ok:
        with open(RESULTS, 'a') as f:
            f.write(f"{key}	{t['dedup']}		failed
")
        # do not record failure in state — the next run must retry it
    if os.path.exists(path):
        os.remove(path)
    save()
    print(f'{n}/{len(todo)} {name} {"done" if ok else "FAILED"}')

print('finished — results.tsv lists the failures; re-run to retry them')
"####;

    let urls_cell = r####"
# 2 — download URLs (never put these in Drive: they expire)
URLS = """paste the contents of the local urls.tsv here"""
url_map = {}
for line in URLS.splitlines():
    parts = line.split('	')
    if len(parts) >= 2 and parts[0] != 'media_key':
        url_map[parts[0]] = parts[1]
print(f'{len(url_map)} URLs ready')
"####;

    let cells: [String; 3] = [py.to_string(), urls_cell.to_string(), run.to_string()];
    let mut body = String::new();
    for (i, src) in cells.iter().enumerate() {
        body.push_str(&format!(
            r#"{{"cell_type":"code","execution_count":null,"metadata":{{}},"outputs":[],"source":{}}}"#,
            serde_json::to_string(src).unwrap_or_else(|_| "\"\"".into())
        ));
        if i + 1 < cells.len() {
            body.push(',');
        }
    }
    format!(
        r#"{{"cells":[{body}],"metadata":{{"colab":{{"provenance":[]}},"kernelspec":{{"name":"python3","display_name":"Python 3"}}}},"nbformat":4,"nbformat_minor":0}}"#,
        body = body
    )
}

// one item Google says takes storage, with everything the cheap re-upload needs
#[derive(Clone, Default)]
struct StorageRow {
    key: String,
    dedup: String,
    thumb: String,
    ts: i64,
    size: u64,
    filename: String,
}

fn human_size(n: u64) -> String {
    if n > 1_073_741_824 {
        format!("{:.2} GB", n as f64 / 1_073_741_824.0)
    } else if n > 1_048_576 {
        format!("{:.1} MB", n as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KB", n as f64 / 1024.0)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // drag photos/videos anywhere in the app: stage them for review, never
        // start an upload without the explicit Start button
        let dropped: Vec<std::path::PathBuf> = {
            let files = ctx.input(|i| i.raw.dropped_files.clone());
            dropped_file_paths(files.into_iter().filter_map(|f| f.path).collect())
        };
        if !dropped.is_empty() {
            self.upload_queue.extend(dropped);
            self.upload_popup = true;
        }
        let mut want_keys: Option<bool> = None;
        self.hover_want = None;
        // ctrl+wheel over the grid resizes thumbnails, the way a file manager
        // does; the wheel delta is consumed so the page does not scroll too
        if self.page.lock().unwrap().as_str() == "photos" {
            let mut step = 0.0f32;
            ctx.input_mut(|i| {
                if i.modifiers.command && i.raw_scroll_delta.y.abs() > 0.5 {
                    step = if i.raw_scroll_delta.y > 0.0 { 8.0 } else { -8.0 };
                    i.raw_scroll_delta = Vec2::ZERO;
                }
            });
            if step != 0.0 {
                self.thumb_size = (self.thumb_size + step).clamp(96.0, 220.0);
                set_setting("thumb_size", &format!("{}", self.thumb_size as i32));
            }
        }
        // `?` opens the key card; esc closes it, and typing in a field is left alone
        self.jump_hook = std::env::var("GPHOTOS_JUMP").ok();
        let mut want_jump = None;
        let mut want_sel_action: Option<&str> = None;
        let mut want_select_all = false;
        let mut want_clear_sel = false;
        let selected_is_fav = {
            let sel = self.lib_selected.lock().unwrap();
            let items = self.lib_items.lock().unwrap();
            items
                .iter()
                .find(|i| sel.contains(&i.media_key))
                .map(|i| i.is_favorite)
                .unwrap_or(false)
        };
        let in_archive_view = self.current_lib_view() == "archive";
        if self.jump_open {
            // the jump box owns the keyboard while it is up
        } else if !ctx.wants_keyboard_input() {
            ctx.input(|i| {
                if i.key_pressed(egui::Key::Questionmark) {
                    want_keys = Some(!self.show_keys);
                } else if i.key_pressed(egui::Key::Escape) && self.show_keys {
                    want_keys = Some(false);
                } else if i.key_pressed(egui::Key::G) && self.viewer.is_none() {
                    want_jump = Some(true);
                }
                // act on the current selection without reaching for the mouse
                if self.viewer.is_none() && !self.lib_selected.lock().unwrap().is_empty() {
                    if i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace) {
                        want_sel_action = Some("trash");
                    } else if i.key_pressed(egui::Key::F) {
                        want_sel_action = Some(if selected_is_fav { "unfav" } else { "fav" });
                    } else if i.key_pressed(egui::Key::E) {
                        want_sel_action = Some(if in_archive_view { "unarchive" } else { "archive" });
                    }
                }
                // grid shortcuts; page-gated so other pages keep their own keys
                if self.viewer.is_none() && self.page.lock().unwrap().as_str() == "photos" {
                    if i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::A) {
                        want_select_all = true;
                    } else if i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::A) {
                        want_clear_sel = true;
                    } else if i.key_pressed(egui::Key::Escape) && !self.show_keys {
                        // with the key card up, Escape closes the card instead
                        want_clear_sel = true;
                    }
                }
            });
            if want_select_all {
                self.select_all();
            }
            if want_clear_sel {
                self.lib_selected.lock().unwrap().clear();
            }
            if let Some(v) = want_keys {
                self.show_keys = v;
            }
        }
        if self.show_keys {
            keys_card(ctx);
        }
        if let Some(a) = want_sel_action {
            self.lib_action(a);
        }
        if let Some(v) = want_jump {
            self.jump_open = v;
            self.jump_text.clear();
        }
        self.jump_ui(ctx);
        if self.restore_window {
            self.restore_window = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        // drain action messages
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Undo(action, keys) => {
                    self.undo = Some((action, keys, Instant::now()));
                }
                Msg::Toast { msg, is_error } => {
                    self.pending_toasts.push((msg, is_error, Instant::now()));
                }
                Msg::PickedDirs(paths) => {
                    for p in paths {
                        if !self.dirs_text.lines().any(|l| l.trim() == p) {
                            if !self.dirs_text.trim().is_empty() {
                                self.dirs_text.push('\n');
                            }
                            self.dirs_text.push_str(&p);
                        }
                    }
                    let t = self.dirs_text.clone();
                    self.save_dirs(t);
                }
                Msg::Token(t) => self.add_creds(t),
                Msg::SignInDone => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Msg::SignedIn => {
                    // clear the logout flag first, then derive the (jar,email)
                    // from the browser session that just completed sign-in
                    set_setting("logged_out", "0");
                    let sel = load_selected_identity();
                    if let Some(sel) = sel {
                        self.do_switch(sel);
                    } else {
                        let _ = self.tx.send(Msg::Toast {
                            msg: "signed in, but no Google Photos session was found yet — retry in a moment".into(),
                            is_error: true,
                        });
                    }
                }
                Msg::Lib(items, view, next_token) => {
                    // the full-timeline measurement means "everything loaded"
                    let full = items.len() > 1000;
                    // a late network reply for another view must not clobber
                    // the grid the user is currently looking at
                    if view == self.current_lib_view() {
                        *self.lib_next.lock().unwrap() = if view == "recent" && full {
                            None
                        } else {
                            next_token
                        };
                        *self.lib_items.lock().unwrap() = items;
                        self.items_gen.fetch_add(1, Ordering::Relaxed);
                        *self.lib_view.lock().unwrap() = view;
                        self.album_selected.clear();
                        self.lib_selected.lock().unwrap().clear();
                        self.sel_anchor = None;
                        self.tex.clear();
                    }
                }
                // the fetch worker already stored the tile list
                Msg::Albums(_, ref v) if v == &self.current_lib_view() => {
                    self.lib_selected.lock().unwrap().clear();
                    self.album_selected.clear();
                    self.sel_anchor = None;
                }
                Msg::Things(_, ref v) if v == &self.current_lib_view() => {
                    self.lib_selected.lock().unwrap().clear();
                    self.album_selected.clear();
                    self.sel_anchor = None;
                }
                Msg::Links(_, ref v) if v == &self.current_lib_view() => {
                    self.lib_selected.lock().unwrap().clear();
                    self.album_selected.clear();
                    self.sel_anchor = None;
                }
                // late reply from a view the user already left: ignore it
                Msg::Albums(_, _) | Msg::Links(_, _) | Msg::Things(_, _) => {}
                Msg::VideoUrl(key, url) => {
                    if !url.is_empty() {
                        cache_video_async(&key, &url);
                        self.vid_urls
                            .lock()
                            .unwrap()
                            .insert(key.clone(), (url.clone(), Instant::now()));
                    }
                    let mut started = false;
                    if let Some(v) = self.vid.as_mut() {
                        if v.key == key && !url.is_empty() {
                            v.url = url.clone();
                            started = true;
                        }
                    }
                    let showing = self.viewer.as_deref() == Some(key.as_str());
                    if showing && started {
                        let dur = self.vid.as_ref().map(|v| v.dur).unwrap_or(0.0);
                        let from = self.resume_at(&key, dur).unwrap_or(0.0);
                        if from > 0.0 {
                            self.resume_note = Some((key.clone(), from, Instant::now()));
                        }
                        self.video_start(from);
                    }
                }
                Msg::Thumb(key, img) => {
                    // 600 (~90 MB) keeps the app responsive on a full library.
                    // The old 3000 cap (~450 MB) made RAM climb into the
                    // hundreds of MB and looked like a freeze. Evicted tiles
                    // re-decode from the disk cache in the 5 s fill pass.
                    const MAX_THUMBS: usize = 1200;
                    let mut map = self.thumbs.lock().unwrap();
                    // only track a key once: duplicate entries in the LRU made
                    // the loop evict still-visible tiles, leaving blank grids
                    if !map.contains_key(&key) {
                        self.thumb_order.push_back(key.clone());
                    }
                    map.insert(key, img);
                    while map.len() > MAX_THUMBS {
                        if let Some(old) = self.thumb_order.pop_front() {
                            if map.remove(&old).is_some() {
                                // drop the GPU texture with its RAM image
                                self.tex.remove(&old);
                            }
                        } else {
                            break;
                        }
                    }
                }
                Msg::RefreshLib => {
                    let v = self.current_lib_view();
                    let local = v == "dupes" || v == "videos" || v == "photos" || v == "large"
                        || v.starts_with("filter:") || v.starts_with("year:") || v.starts_with("month:")
                        || v.starts_with("type:") || v.starts_with("filename:");
                    // local subsets are built by show_subset, not fetch_lib;
                    // refresh the canonical grid instead of blanking them
                    self.fetch_lib(if local { "recent".into() } else { v });
                }
                Msg::SessionReady(ok) => {
                    *self.page.lock().unwrap() = if ok { "photos".into() } else { "signin".into() };
                    if ok {
                        // an album link owns the first grid fetch — the usual
                        // "recent" load would otherwise overwrite it
                        let album_link = match self.pending_deeplink.as_ref() {
                            Some((Kind::Album(_), key)) => Some(key.clone()),
                            _ => None,
                        };
                        if let Some(key) = album_link {
                            self.pending_deeplink = None;
                            self.fetch_lib(format!("album:{}", key));
                        } else {
                            self.fetch_lib("recent".into());
                        }
                        self.fetch_quota(); // storage shows without a click
                        // one full measurement per launch, after sign-in settles
                        if std::env::var("GPHOTOS_HOGS").is_err() {
                            self.scan_storage(false);
                        }
                    }
                }
                Msg::Viewer(key, img) => {
                    if self.viewer.as_deref() == Some(key.as_str()) {
                        self.tex.remove(&format!("full:{}", key));
                        *self.viewer_img.lock().unwrap() = Some((key, img));
                    } else {
                        // a late full-image load for a photo already closed:
                        // drop it instead of leaving a 22 MB image/texture alive
                        self.tex.remove(&format!("full:{}", key));
                    }
                }
                Msg::ViewerInfo(info) => {
                    if self.viewer.as_deref() == Some(info.media_key.as_str()) {
                        *self.viewer_info.lock().unwrap() = Some(info);
                    }
                }
                Msg::ViewerExt(key, ext) => {
                    // fast nav: a late reply for the previous photo must not
                    // paint its camera/place into the current photo's panel
                    if self.viewer.as_deref() == Some(key.as_str()) {
                        *self.viewer_ext.lock().unwrap() = Some(ext);
                    }
                }
                Msg::Accounts(discovered) => {
                    // Persisted rows keep the owner's choices; new identities
                    // are staged for confirmation and never written here.
                    let existing = library::load_registry();
                    let (mut rows, staged) = merge_accounts(&existing, &discovered);
                    // keep the currently signed-in identity visible even with
                    // no registry row and no live discovery result
                    if let Some((jar, email)) = self.selected.clone() {
                        if !rows.iter().any(|r| r.jar == jar && r.email == email) {
                            rows.push(library::AccountRecord {
                                jar,
                                email,
                                name: String::new(),
                                hidden: false,
                                last_seen: 0,
                                last_ok: 0,
                            });
                        }
                    }
                    rows.extend(staged);
                    rows.sort_by(|a, b| a.jar.cmp(&b.jar).then(a.email.cmp(&b.email)));
                    if self.add_jar.is_empty() {
                        self.add_jar = self
                            .selected
                            .as_ref()
                            .map(|(j, _)| j.clone())
                            .or_else(|| rows.first().map(|r| r.jar.clone()))
                            .unwrap_or_default();
                    }
                    *self.switch_list.lock().unwrap() = rows;
                }
                Msg::PendingAccounts(rows) => {
                    for r in rows {
                        if !is_hidden(&self.hidden, &r.jar, &r.email)
                            && !self.pending_adds.iter().any(|p| same_account(p, &r))
                        {
                            self.pending_adds.push(r);
                        }
                    }
                    self.add_poll = None;
                }
                Msg::Sizes(pairs) => {
                    let mut m = self.sizes.lock().unwrap();
                    for (k, v) in pairs {
                        m.insert(k, v);
                    }
                }
                Msg::StorageStats(st) => self.hog_stats = Some(*st),
                Msg::AllItems(items) => {
                    *self.all_items.lock().unwrap() = items.clone();
                    if self.current_lib_view() == "hogs" {
                        *self.lib_items.lock().unwrap() = items;
                        self.items_gen.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Msg::BulkInfo(map) => {
                    *self.bulk.lock().unwrap() = map;
                }
                Msg::MoreLib(items, view, next_token) => {
                    self.warm_active = false;
                    if view != self.current_lib_view() {
                        return; // late page from a view we already left
                    }
                    *self.lib_next.lock().unwrap() = next_token;
                    let mut all = self.lib_items.lock().unwrap();
                    let before = all.len();
                    let existing: std::collections::HashSet<String> =
                        all.iter().map(|x| x.media_key.clone()).collect();
                    for it in items {
                        if !existing.contains(&it.media_key) {
                            all.push(it);
                        }
                    }
                    if all.len() != before {
                        self.items_gen.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // keep the sketch lines gently alive (cloud bob) and poll snapshots
        // 20 fps keeps the gentle sketch animations smooth without burning CPU
        let vid_live = self.vid.as_ref().map(|v| v.playing).unwrap_or(false);
        ctx.request_repaint_after(Duration::from_millis(if vid_live { 25 } else { 50 }));
        if self.snap_dirty.swap(false, Ordering::Relaxed) {
            let s = self.snap.lock().unwrap();
            if self.dirs_text.trim().is_empty() && !s.dirs.is_empty() {
                self.dirs_text = s.dirs.join("\n");
            }
        }

        let (watcher, account, uploads_n, retries, db_mb, unit_ok, today_n) = {
            let s = self.snap.lock().unwrap();
            (
                s.watcher,
                s.account.clone(),
                s.total_uploads,
                s.retries,
                s.db_mb,
                s.unit_installed,
                s.today_uploads,
            )
        };

        // 20 fps keeps the sketch animations smooth without burning CPU
        ctx.request_repaint_after(Duration::from_millis(50));
        if self.snap_dirty.swap(false, Ordering::Relaxed) {
            let s = self.snap.lock().unwrap();
            if self.dirs_text.trim().is_empty() && !s.dirs.is_empty() {
                self.dirs_text = s.dirs.join("\n");
            }
        }

        // test hook: GPHOTOS_PAGE=settings boots straight into a page
        if let Ok(p) = std::env::var("GPHOTOS_PAGE") {
            *self.page.lock().unwrap() = p;
        }
        // test hook: GPHOTOS_STARTVIEW=<view> boots into a view for screenshots
        if let Ok(v) = std::env::var("GPHOTOS_STARTVIEW") {
            let cur = self.lib_view.lock().unwrap().clone();
            if cur != v && self.lib_items.lock().unwrap().is_empty() {
                self.fetch_lib(v);
            }
        }
        // test hook: GPHOTOS_AUTOOPEN=video|photo opens one on launch, so the
        // viewer can be checked without touching the mouse
        if let Ok(k) = std::env::var("GPHOTOS_OPEN") {
            if self.viewer.is_none() {
                self.open_viewer(&k);
            }
        }
        if let Ok(kind) = std::env::var("GPHOTOS_AUTOOPEN") {
            if self.viewer.is_none() {
                let items = self.lib_items.lock().unwrap().clone();
                let want_video = kind != "photo";
                if let Some(it) = items.iter().find(|i| i.is_video == want_video) {
                    self.open_viewer(&it.media_key);
                }
            }
        }
        // launch deeplink: a photo opens as soon as the boot library fetch has
        // that key (no new RPC for one item — the GPHOTOS_OPEN hook does the
        // same). Albums are launched from SessionReady so the first fetch is
        // the album, not the default timeline.
        if let Some((Kind::Photo(_), key)) = self.pending_deeplink.clone() {
            // drop the lock before open_viewer, which locks lib_items again
            let known = self.lib_items.lock().unwrap().iter().any(|i| i.media_key == key);
            if known {
                self.open_viewer(&key);
                self.pending_deeplink = None;
            }
        }
        // screenshot hook: open the measured page once the session is ready
        if std::env::var("GPHOTOS_HOGS").is_ok()
            && !self.lib_busy.load(Ordering::Relaxed)
            && self.current_lib_view() != "hogs"
            && self.hog_stats.is_some()
        {
            self.fetch_hogs();
        }
        if let Some(path) = self.shot_path.clone() {
            if self.shot_at.elapsed() > Duration::from_secs(3) {
                self.shot_at = Instant::now();
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
            let shot = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(img) = shot {
                let (w, h) = (img.width() as u32, img.height() as u32);
                let mut buf = Vec::with_capacity((w * h * 4) as usize);
                for px in &img.pixels {
                    buf.extend_from_slice(&[px.r(), px.g(), px.b(), px.a()]);
                }
                if let Some(rgba) = image::RgbaImage::from_raw(w, h, buf) {
                    let _ = rgba.save(&path);
                }
            }
        }
        let page = self.page.lock().unwrap().clone();
        match page.as_str() {
            "signin" => self.signin_page(ctx),
            "sync" => self.sync_page(ctx),
            "settings" => self.settings_page(ctx),
            _ => self.photos_page(ctx),
        }
        // the full-screen photo viewer sits on top of whichever page is open
        if self.viewer.is_some()
            && self.vid.is_none()
            && self.slideshow_secs > 0.0
            && self.slideshow_at.elapsed()
                >= Duration::from_millis((self.slideshow_secs * 1000.0) as u64)
        {
            self.slideshow_at = Instant::now();
            self.viewer_nav(1);
        }
        if self.viewer.is_some() {
            self.viewer_ui(ctx);
        }
        if self.switch_open {
            self.switch_ui(ctx);
        }
        self.upload_popup_ui(ctx);
        self.storage_ui(ctx);
        self.jump_pump();
        self.warm_pump();
        // after the grid has reported what is under the pointer
        self.hover_tick(ctx);
        self.render_toasts(ctx);
    }

}

// date label for grid grouping — Today / Yesterday / 12 Nov 2025
fn date_label(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = ts.div_euclid(86400);
    let now_days = now.div_euclid(86400);
    let (y, m, d) = library::civil_from_days(days);
    if days == now_days {
        "Today".into()
    } else if days == now_days - 1 {
        "Yesterday".into()
    } else {
        const MONTHS: [&str; 12] = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];
        format!("{} {} {}", d, MONTHS[(m - 1) as usize], y)
    }
}


impl App {
    // ---- pages ---------------------------------------------------------------

    // the old dashboard: folders, watcher controls, settings, accounts, history

    // ---- settings: disk caches ------------------------------------------------
    // ---- settings: storage caches, locked folder visibility -----------------
    fn settings_page(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        if ui.button("< photos").clicked() {
                            *self.page.lock().unwrap() = "photos".into();
                        }
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("Settings").size(28.0).strong().color(INK));
                    });
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new("what this app keeps, and what it shows")
                                .size(13.5)
                                .color(INK_SOFT),
                        );
                    });
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(14.0);

                    let w = (ui.available_width() - 40.0).clamp(320.0, 780.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.vertical(|ui| {
                            ui.set_width(w);
                            self.storage_card(ui);
                            ui.add_space(14.0);
                            self.look_card(ui);
                            ui.add_space(14.0);
                            self.controls_card(ui);
                            ui.add_space(14.0);
                            self.upload_card(ui);
                            ui.add_space(14.0);
                            self.cache_card(ui);
                            ui.add_space(14.0);
                            self.locked_card(ui);
                        });
                    });
                    ui.add_space(24.0);
                });
        });
    }

    // ---- timeline rail + jump to a date --------------------------------------

    fn month_buckets(&mut self) -> Vec<MonthBucket> {
        let key = self.all_items.lock().unwrap().len();
        if key == self.month_cache_key && !self.month_cache.is_empty() {
            return self.month_cache.clone();
        }
        // use the whole measured library, not just the pages on screen: the
        // rail has to be able to reach a month that has not been paged in yet
        let measured = self.all_items.lock().unwrap().clone();
        let mut items: Vec<(i64, String)> = if measured.is_empty() {
            self.lib_items
                .lock()
                .unwrap()
                .iter()
                .filter(|i| i.ts > 0)
                .map(|i| (i.ts, i.media_key.clone()))
                .collect()
        } else {
            measured
                .iter()
                .filter(|i| i.ts > 0)
                .map(|i| (i.ts, i.media_key.clone()))
                .collect()
        };
        items.sort_by(|a, b| b.0.cmp(&a.0));
        let mut out: Vec<MonthBucket> = vec![];
        for (ts, key) in items {
            let (y, m, _) = library::civil_from_days(ts.div_euclid(86400));
            match out.last_mut() {
                Some(b) if b.year == y && b.month == m => b.count += 1,
                _ => out.push(MonthBucket {
                    year: y,
                    month: m,
                    count: 1,
                    first: key,
                    ts,
                }),
            }
        }
        self.month_cache_key = key;
        self.month_cache = out.clone();
        out
    }

    // Immich's scrubber: a narrow rail whose segments are sized by how many
    // items a month holds, so a year of photos is one glance and one click.
    fn timeline_rail(&mut self, ui: &mut egui::Ui, buckets: &[MonthBucket]) {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        let total: usize = buckets.iter().map(|b| b.count).sum();
        if total == 0 {
            return;
        }
        let p = ui.painter();
        // paper track, native palette (the panel has no frame of its own)
        p.rect_filled(rect, 0.0, PAPER);
        let track = Rect::from_min_size(
            Pos2::new(rect.right() - 6.0, rect.top() + 2.0),
            Vec2::new(3.0, (rect.height() - 4.0).max(10.0)),
        );
        p.rect_filled(track, 2.0, Color32::from_rgb(0xe4, 0xdb, 0xc6));

        // how much of the timeline is on screen decides the handle length
        let view = self.grid_view_h.max(1.0);
        let content = self.grid_content_h.max(view);
        let span_frac = (view / content).clamp(0.06, 1.0);
        let handle_h = (track.height() * span_frac).max(14.0).min(track.height());
        let travel = (track.height() - handle_h).max(0.0);
        let frac = self.grid_frac.clamp(0.0, 1.0);
        let hy = track.top() + frac * travel;
        let handle = Rect::from_min_size(
            Pos2::new(track.left() - 1.0, hy),
            Vec2::new(track.width() + 2.0, handle_h),
        );
        let hot = resp.hovered() || self.grid_drag;
        p.rect_filled(
            handle,
            3.0,
            if hot {
                Color32::from_rgb(0xbf, 0xb0, 0x8e)
            } else {
                Color32::from_rgb(0xc9, 0xbe, 0xa6)
            },
        );

        // drag / click maps pointer y directly to the scroll fraction
        if resp.clicked() || resp.dragged() || resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos().or_else(|| resp.hover_pos()) {
                let f = ((pos.y - track.top() - handle_h * 0.5) / travel.max(1.0))
                    .clamp(0.0, 1.0);
                self.grid_frac = f;
                let max = (self.grid_content_h - self.grid_view_h).max(0.0);
                self.grid_want = Some(scroll_fraction_to_offset(
                    f,
                    self.grid_content_h,
                    self.grid_view_h,
                ));
                if std::env::var("GPHOTOS_SCROLLDBG").is_ok() {
                    eprintln!("[SD] rail drag f={:.3} max={:.0} off={:.0}", f, max, f * max);
                }
            }
        }
        self.grid_drag = resp.dragged();
        // double-click a month on the rail to open just that year
        if resp.double_clicked() {
            if let Some(py) = resp.interact_pointer_pos().map(|p| p.y).or_else(|| resp.hover_pos().map(|p| p.y)) {
                let total = buckets.iter().map(|b| b.count).sum::<usize>().max(1) as f32;
                let mut acc = 0usize;
                for b in buckets {
                    acc += b.count;
                    if (acc as f32 / total) >= ((py - rect.top()) / rect.height()).clamp(0.0, 1.0) {
                        let y = b.year;
                        let items: Vec<library::MediaItem> = self
                            .all_items
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|i| {
                                i.ts > 0
                                    && library::civil_from_days(i.ts.div_euclid(86400)).0 == y
                            })
                            .cloned()
                            .collect();
                        if !items.is_empty() {
                            self.show_subset(&format!("year:{}", y), items, true);
                        }
                        break;
                    }
                }
            }
        }

        // hover bubble: which month is under the pointer (year only on hover)
        let pointer_y = std::env::var("GPHOTOS_RAILHOVER")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .map(|f| rect.top() + rect.height() * f)
            .or_else(|| resp.hover_pos().map(|p| p.y));
        // the visible scroll fraction maps to a calendar month when the whole
        // timeline is loaded, otherwise use the pointer position as before
        let lookup = if self.grid_content_h > self.grid_view_h * 1.5 {
            let max = (self.grid_content_h - self.grid_view_h).max(0.0);
            let off = self.grid_frac * max;
            rect.top() + (off / max.max(1.0)) * rect.height()
        } else {
            pointer_y.unwrap_or(rect.top())
        };
        let mut acc = 0usize;
        let mut hit: Option<&MonthBucket> = None;
        for b in buckets {
            acc += b.count;
            if (acc as f32 / total as f32) >= ((lookup - rect.top()) / rect.height()).clamp(0.0, 1.0) {
                hit = Some(b);
                break;
            }
        }
        if let (Some(b), Some(py)) = (hit, pointer_y) {
            let tip = format!("{} {} · {}", month_name(b.month), b.year, b.count);
            let painter = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("rail_tip"),
            ));
            let font = egui::FontId::proportional(12.5);
            let galley = painter.layout_no_wrap(tip, font, INK);
            let bubble = Rect::from_min_size(
                Pos2::new(
                    rect.right() - 26.0 - galley.size().x - 10.0,
                    (py - 13.0).clamp(rect.top(), rect.bottom() - 26.0),
                ),
                Vec2::new(galley.size().x + 14.0, 26.0),
            );
            painter.rect_filled(bubble, 13.0, PAPER);
            painter.rect_stroke(bubble, 13.0, Stroke::new(1.4, INK));
            painter.galley(
                Pos2::new(bubble.left() + 7.0, bubble.center().y - galley.size().y * 0.5),
                galley,
                INK,
            );
        }
    }

    // the `g` box: type a year, a month, or a day and land there
    fn jump_ui(&mut self, ctx: &egui::Context) {
        if !self.jump_open {
            return;
        }
        let mut open = true;
        let mut go = false;
        let parsed = parse_date_query(&self.jump_text);
        let mut close = false;
        egui::Window::new(egui::RichText::new("Jump to a date").size(16.0).strong().color(INK))
            .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .frame(
                egui::Frame::none()
                    .fill(PAPER)
                    .stroke(Stroke::new(1.5, INK))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(14.0)),
            )
            .show(ctx, |ui| {
                ui.set_width(320.0);
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.jump_text)
                        .hint_text("2019, jul 2019, 15 jul 2019, 2019-07-15")
                        .desired_width(300.0),
                );
                if !edit.has_focus() && self.jump_text.is_empty() {
                    edit.request_focus();
                }
                ui.add_space(4.0);
                let line = match parsed {
                    Some(ts) => format!("goes to {}", date_label(ts)),
                    None => "type a year, a month and year, or a day".to_string(),
                };
                ui.label(egui::RichText::new(line).size(12.5).color(INK_SOFT));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    go = ui
                        .add_enabled(parsed.is_some(), egui::Button::new("jump"))
                        .clicked();
                    if ui.button("cancel").clicked() {
                        close = true;
                    }
                });
                // Enter works while the field still has focus (the common case)
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = parsed.is_some();
                }
            });
        if go {
            self.jump_target = parsed;
            close = true;
        }
        if close {
            self.jump_open = false;
            self.jump_text.clear();
        }
        self.jump_open = self.jump_open && open;
    }

    // page in until the wanted day is loaded, then send the grid to it
    fn jump_pump(&mut self) {
        let Some(target) = self.jump_target else { return };
        if std::env::var("GPHOTOS_SCROLLDBG").is_ok() {
            eprintln!("[SD] pump target={} view={} busy={} next={} lib={} all={}", target, self.current_lib_view(), self.lib_busy.load(Ordering::Relaxed), self.lib_next.lock().unwrap().is_some(), self.lib_items.lock().unwrap().len(), self.all_items.lock().unwrap().len());
        }
        if self.current_lib_view() != "recent" {
            self.jump_target = None;
            return;
        }
        if self.lib_busy.load(Ordering::Relaxed) {
            return;
        }
        let (hit, oldest) = {
            let items = self.lib_items.lock().unwrap();
            (
                items
                    .iter()
                    .find(|i| i.ts > 0 && i.ts <= target)
                    .map(|i| i.media_key.clone()),
                items
                    .iter()
                    .filter(|i| i.ts > 0)
                    .map(|i| i.ts)
                    .min()
                    .unwrap_or(i64::MAX),
            )
        };
        if std::env::var("GPHOTOS_SCROLLDBG").is_ok() {
            eprintln!("[SD] pump decision hit={:?} oldest={} next={}", hit, oldest, self.lib_next.lock().unwrap().is_some());
        }
        if let Some(k) = hit {
            self.scroll_anchor = Some(k);
            self.jump_target = None;
        } else if oldest > target && self.lib_next.lock().unwrap().is_some() {
            self.fetch_more();
        } else {
            self.jump_target = None;
        }
    }

    // a rail jump lands on tiles whose thumbnails were never fetched; pull
    // the neighbourhood right away instead of waiting for the sequential filler
    fn prewarm_thumbs_around(&self, key: &str) {
        let Some(s) = self.lib_session.lock().unwrap().clone() else { return };
        let items = self.lib_items.lock().unwrap().clone();
        let Some(i) = items.iter().position(|x| x.media_key == key) else { return };
        let jobs: Vec<(String, String)> = items[i..items.len().min(i + 60)]
            .iter()
            .map(|x| (x.media_key.clone(), x.thumb.clone()))
            .filter(|(_, u)| !u.is_empty())
            .collect();
        let tx = self.tx.clone();
        thread::spawn(move || fetch_thumbs_for(&s, &jobs, &tx));
    }

    // keep paging the timeline in the background so the scroll range (and the
    // date rail) reaches the bottom on a fresh session, for every account
    fn warm_pump(&mut self) {
        if self.current_lib_view() != "recent"
            || !self.all_items.lock().unwrap().is_empty()
            || self.jump_target.is_some()
            || self.lib_busy.load(Ordering::Relaxed)
            || self.lib_next.lock().unwrap().is_none()
            || self.last_warm.elapsed() < Duration::from_millis(600)
        {
            return;
        }
        self.last_warm = Instant::now();
        self.warm_active = true;
        self.fetch_more();
    }

    // the measured numbers, in plain words: the same lines --storage-report prints
    fn storage_report(&self, ui: &mut egui::Ui, st: &library::StorageStats) {
        egui::Frame::none()
            .fill(PAPER.gamma_multiply(0.97))
            .stroke(Stroke::new(1.0, INK_SOFT.gamma_multiply(0.5)))
            .rounding(egui::Rounding::same(9.0))
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, |ui| {
                let mut lines = vec![(
                    "what Google's counter says".to_string(),
                    format!(
                        "{} — this is the account tally, not the sum of the files below",
                        self.quota_txt.lock().unwrap().clone()
                    ),
                )];
                lines.extend(library::stats_lines(st));
                // file types we can see from the filenames the scan kept
                let mut by_ext: std::collections::BTreeMap<String, (usize, u64)> =
                    Default::default();
                {
                    let bulk = self.bulk.lock().unwrap();
                    let sizes = self.sizes.lock().unwrap();
                    for it in self.all_items.lock().unwrap().iter() {
                        let name = bulk.get(&it.media_key).map(|x| x.0.as_str()).unwrap_or("");
                        let ext = name
                            .rsplit_once('.')
                            .map(|(_, e)| e.to_ascii_lowercase())
                            .unwrap_or_default();
                        let e = by_ext
                            .entry(if ext.is_empty() { "?".into() } else { ext })
                            .or_default();
                        e.0 += 1;
                        e.1 += sizes.get(&it.media_key).copied().unwrap_or(0);
                    }
                }
                if !by_ext.is_empty() {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("by file type").size(13.0).strong().color(INK));
                    let mut types: Vec<_> = by_ext.into_iter().collect();
                    types.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
                    ui.horizontal_wrapped(|ui| {
                        for (ext, (n, b)) in types.into_iter().take(12) {
                            if ui
                                .small_button(format!("{} · {} · {}", ext, n, library::human(b)))
                                .clicked()
                            {
                                let items: Vec<library::MediaItem> = {
                                    let bulk = self.bulk.lock().unwrap();
                                    self.all_items
                                        .lock()
                                        .unwrap()
                                        .iter()
                                        .filter(|it| {
                                            let name = bulk
                                                .get(&it.media_key)
                                                .map(|x| x.0.as_str())
                                                .unwrap_or("");
                                            name.rsplit_once('.')
                                                .map(|(_, e)| e.eq_ignore_ascii_case(&ext))
                                                .unwrap_or(ext == "?")
                                        })
                                        .cloned()
                                        .collect()
                                };
                                self.show_subset(&format!("type:{}", ext), items, true);
                            }
                        }
                    });
                }
                for (i, (label, value)) in lines.into_iter().enumerate() {
                    if i > 0 {
                        ui.add_space(4.0);
                    }
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [150.0, 0.0],
                            egui::Label::new(
                                egui::RichText::new(label).size(12.0).color(INK_SOFT),
                            ),
                        );
                        ui.label(
                            egui::RichText::new(wrap_line(&value, 112))
                                .size(12.5)
                                .color(INK),
                        );
                    });
                }
            });
        ui.add_space(8.0);
    }

    // ---- Colab jobs: the same work, on a machine that is not this one -------

    // A resumable job folder: one line per item, a notebook that walks it and
    // checkpoints after every file, and a README that says how to start it.
    // Same shape gallery-dl and rclone use for archives, for the same reason:
    // a disconnect must cost one item, not the run.
    fn colab_dir() -> std::path::PathBuf {
        preview_dir()
            .parent()
            .map(|p| p.join("colab-job"))
            .unwrap_or_else(|| std::env::temp_dir().join("gphotos-colab-job"))
    }

    fn write_colab_job(rows: &[StorageRow]) -> Option<std::path::PathBuf> {
        Self::write_colab_job_at(Self::colab_dir(), rows)
    }

    fn write_colab_job_at(dir: std::path::PathBuf, rows: &[StorageRow]) -> Option<std::path::PathBuf> {
        std::fs::create_dir_all(&dir).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let mut tasks = String::from("media_key\tdedup\tts\tsize\tfilename\n");
        let mut urls = String::from("media_key\turl\n");
        for r in rows {
            tasks.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                r.key,
                r.dedup,
                r.ts,
                r.size,
                r.filename.replace('\t', " "),
            ));
            urls.push_str(&format!("{}\t{}\n", r.key, sized_url(&r.thumb, "=d")));
        }
        std::fs::write(dir.join("tasks.tsv"), tasks).ok()?;
        // signed URLs expire: keep them local, never in the Drive bundle
        write_secret_file(&dir.join("urls.tsv"), &urls).ok()?;
        // the README tells the user to upload this binary with the bundle —
        // a job without it is incomplete, so fail instead of reporting success
        let bin_dst = dir.join("gotohp-cli_amd64");
        std::fs::copy(gotohp_bin(), &bin_dst).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&bin_dst, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::write(dir.join("gphotos_free.ipynb"), colab_notebook(rows.len())).ok()?;
        std::fs::write(
            dir.join("README.txt"),
            format!(
                "Make photos free — Colab run\n\
                 =============================\n\n\
                 1. Upload ONLY tasks.tsv, gphotos_free.ipynb and\n\
                    gotohp-cli_amd64 to Google Drive, e.g. MyDrive/gphotos-job/.\n\
                    NEVER upload urls.tsv — it holds signed links that expire.\n\
                 2. Open gphotos_free.ipynb in Colab on the account you are\n\
                    signed in with.\n\
                 3. Run the first cell (mounts Drive and checks the uploader).\n\
                 4. Run the second cell and paste the CONTENTS of the local\n\
                    urls.tsv into URLS. Regenerate this job right before you run\n\
                    it — those signed links expire.\n\
                 5. In the third cell, paste the cookie header into COOKIE:\n\
                        python3 fifecookies.py        (on the computer)\n\
                    Then run it. state.json skips finished rows; failed rows are\n\
                    retried on the next run, and results.tsv records failures.\n\n\
                 {} item(s) in this job.\n",
                rows.len()
            ),
        )
        .ok()?;
        Some(dir)
    }

    fn colab_export(&self, rows: &[StorageRow]) {
        match Self::write_colab_job(rows) {
            Some(dir) => {
                let msg = format!(
                    "job written to {} — upload tasks.tsv, the notebook and gotohp-cli to Drive (NOT urls.tsv)",
                    dir.display()
                );
                let tx = self.tx.clone();
                let _ = tx.send(Msg::Toast {
                    msg: truncate(&msg, 150),
                    is_error: false,
                });
            }
            None => {
                let tx = self.tx.clone();
                let _ = tx.send(Msg::Toast {
                    msg: "could not write the job folder".into(),
                    is_error: true,
                });
            }
        }
    }

    // ---- storage: which items are charged, and how to stop paying ----------

    // Ask Google, item by item, which of them take storage. The bulk flag cannot
    // say where an item came from, and the account figure says nothing about
    // which item it belongs to, so the only honest answer is per-item.
    fn storage_scan(&self) {
        if self.storage_busy.swap(true, Ordering::Relaxed) {
            return;
        }
        let sess = self.lib_session.clone();
        let rows = self.storage_rows.clone();
        let note = self.storage_note.clone();
        let busy = self.storage_busy.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let Some(s) = sess.lock().unwrap().clone() else {
                busy.store(false, Ordering::Relaxed);
                let _ = tx.send(Msg::Toast {
                    msg: "no photos.google.com session — sign in first".into(),
                    is_error: true,
                });
                return;
            };
            type Walk =
                fn(&library::Session, Option<&str>) -> (Vec<library::MediaItem>, Option<String>);
            let sources: [(&str, Walk); 5] = [
                ("timeline", library::recent),
                ("archive", library::archive),
                ("locked", library::locked),
                ("favourites", library::favorites),
                ("trash", library::trash),
            ];
            let mut all: Vec<library::MediaItem> = vec![];
            for (_, f) in sources {
                let mut page: Option<String> = None;
                for _ in 0..600 {
                    let (mut got, next) = f(&s, page.as_deref());
                    let n = got.len();
                    all.append(&mut got);
                    page = match next {
                        Some(t) if n > 0 => Some(t),
                        _ => None,
                    };
                    if page.is_none() {
                        break;
                    }
                }
            }
            all.sort_by(|a, b| a.media_key.cmp(&b.media_key));
            all.dedup_by(|a, b| a.media_key == b.media_key);
            let total = all.len();
            if total == 0 {
                *note.lock().unwrap() =
                    "could not reach Google Photos — check your connection and sign-in, then retry".into();
                let _ = tx.send(Msg::Toast {
                    msg: "storage check failed — no items could be read".into(),
                    is_error: true,
                });
                busy.store(false, Ordering::Relaxed);
                return;
            }
            *note.lock().unwrap() = format!("looking at {} items…", total);
            let mut found: Vec<StorageRow> = vec![];
            let mut done = 0usize;
            for group in all.chunks(105) {
                let parts: Vec<&[library::MediaItem]> = group.chunks(35).collect();
                let sr = &s;
                let infos: Vec<Vec<library::BulkInfo>> = thread::scope(|sc| {
                    let hs: Vec<_> = parts
                        .iter()
                        .map(|g| {
                            let keys: Vec<String> =
                                g.iter().map(|i| i.media_key.clone()).collect();
                            sc.spawn(move || library::bulk_info(sr, &keys))
                        })
                        .collect();
                    hs.into_iter().map(|h| h.join().unwrap_or_default()).collect()
                });
                let by: std::collections::HashMap<String, library::BulkInfo> = infos
                    .into_iter()
                    .flatten()
                    .map(|i| (i.media_key.clone(), i))
                    .collect();
                for it in group {
                    let Some(b) = by.get(&it.media_key) else { continue };
                    if b.takes_up_space == Some(true) {
                        found.push(StorageRow {
                            key: it.media_key.clone(),
                            dedup: it.dedup_key.clone(),
                            thumb: it.thumb.clone(),
                            ts: b.timestamp / 1000,
                            size: b.size,
                            filename: b.filename.clone(),
                        });
                    }
                }
                done += group.len();
                let bytes: u64 = found.iter().map(|r| r.size).sum();
                *note.lock().unwrap() = format!(
                    "checked {} of {} · {} take storage ({})",
                    done,
                    total,
                    found.len(),
                    human_size(bytes)
                );
            }
            found.sort_by(|a, b| b.size.cmp(&a.size));
            let bytes: u64 = found.iter().map(|r| r.size).sum();
            *note.lock().unwrap() = if found.is_empty() {
                format!("all {} items checked — not one of them takes storage", total)
            } else {
                format!(
                    "{} of {} items take storage, {} in total",
                    found.len(),
                    total,
                    human_size(bytes)
                )
            };
            *rows.lock().unwrap() = found;
            busy.store(false, Ordering::Relaxed);
            let _ = tx.send(Msg::Toast {
                msg: "storage check finished".into(),
                is_error: false,
            });
        });
    }

    // download the original, keep its date, upload it again through the free
    // endpoint (which is what keeps it off the quota), and bin the charged copy
    // one item, no guard of its own: safe to call many times in one job
    fn make_free_one(
        row: StorageRow,
        bin: &str,
        sess: &Arc<Mutex<Option<library::Session>>>,
        rows: &Arc<Mutex<Vec<StorageRow>>>,
        tx: &mpsc::Sender<Msg>,
    ) -> bool {
            let name = row.filename.replace(['/', '\\'], "_");
            // unpredictable name + create_new: a pre-planted symlink in the
            // temp dir cannot be followed, and the photo bytes are 0600
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let tmp = std::env::temp_dir().join(format!(
                "gphotos-free-{}-{}-{}",
                std::process::id(),
                nonce,
                name
            ));
            let Some(bytes) = fetch_media(&row.thumb, &library::fife_cookie_header(), "=d")
            else {
                let _ = tx.send(Msg::Toast {
                    msg: "could not download the original".into(),
                    is_error: true,
                });
                return false;
            };
            {
                use std::io::Write;
                #[cfg(unix)]
                use std::os::unix::fs::OpenOptionsExt;
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                opts.mode(0o600);
                let wrote = opts.open(&tmp).and_then(|mut f| f.write_all(&bytes));
                if wrote.is_err() {
                    let _ = tx.send(Msg::Toast {
                        msg: "could not write a temporary copy".into(),
                        is_error: true,
                    });
                    return false;
                }
            }
            // the free endpoint stamps the file's own date onto the item
            if let Some(t) = timestamp_from_unix(row.ts) {
                let times = std::fs::FileTimes::new().set_accessed(t).set_modified(t);
                let _ = std::fs::set_times(&tmp, times);
            }
            let (ok, out) = run_cmd(
                Command::new(&bin)
                    .arg("upload")
                    .arg(&tmp)
                    .arg("--no-tui")
                    .arg("-l")
                    .arg("warn"),
            );
            let _ = std::fs::remove_file(&tmp);
            if !ok {
                let last = out.lines().last().unwrap_or("uploader said no").to_string();
                let _ = tx.send(Msg::Toast {
                    msg: format!("re-upload failed: {}", truncate(&last, 120)),
                    is_error: true,
                });
                return false;
            }
            // the uploader prints a mediaKey for the new (free) copy. Only
            // trash the charged original if we actually got one — a silent
            // success with no key must never cost the user their photo.
            let new_key = out
                .lines()
                .rev()
                .find_map(|l| {
                    let i = l.find("\"mediaKey\"")?;
                    let rest = &l[i + 11..];
                    let a = rest.find('"')? + 1;
                    let b = a + rest[a..].find('"')?;
                    Some(rest[a..b].to_string())
                });
            let mut trashed = false;
            if let Some(nk) = &new_key {
                if !row.dedup.is_empty() && nk != &row.key {
                    if let Some(s) = sess.lock().unwrap().clone() {
                        trashed = library::set_trashed(&s, &[row.dedup.clone()], true);
                    }
                }
            }
            if new_key.is_some() {
                rows.lock().unwrap().retain(|r| r.key != row.key);
            }
            let _ = tx.send(Msg::Toast {
                msg: if trashed {
                    format!("{} is free now · old copy in trash", truncate(&row.filename, 40))
                } else if new_key.is_none() {
                    format!("{} uploaded free, but no confirmation key — kept the old copy", truncate(&row.filename, 40))
                } else {
                    format!("{} is free now · delete the old copy by hand", truncate(&row.filename, 40))
                },
                is_error: false,
            });

            new_key.is_some()
    }

    fn reupload_free(&self, row: StorageRow) {
        if row.thumb.is_empty() {
            return;
        }
        if self.engine == "colab" {
            self.colab_export(&[row]);
            return;
        }
        let bin = std::env::var("GPHOTOS_GOTOHP")
            .unwrap_or_else(|_| gotohp_bin().to_string_lossy().into_owned());
        let sess = self.lib_session.clone();
        let rows = self.storage_rows.clone();
        self.spawn_action(move |tx| {
            Self::make_free_one(row, &bin, &sess, &rows, &tx);
        });
    }

    // "make all of them free" is one job: spawn_action's busy guard would let
    // only the first row through if each row started its own action
    fn reupload_free_batch(&self, rows: Vec<StorageRow>) {
        if rows.is_empty() {
            return;
        }
        if self.engine == "colab" {
            self.colab_export(&rows);
            return;
        }
        let bin = std::env::var("GPHOTOS_GOTOHP")
            .unwrap_or_else(|_| gotohp_bin().to_string_lossy().into_owned());
        let sess = self.lib_session.clone();
        let rows_sh = self.storage_rows.clone();
        self.spawn_action(move |tx| {
            let n = rows.len();
            let mut done = 0;
            for row in rows {
                if Self::make_free_one(row, &bin, &sess, &rows_sh, &tx) {
                    done += 1;
                }
            }
            let _ = tx.send(Msg::Toast {
                msg: format!("{done}/{n} items made free"),
                is_error: done == 0,
            });
        });
    }

    fn storage_ui(&mut self, ctx: &egui::Context) {
        if !self.storage_open {
            return;
        }
        let mut open = true;
        let mut scan = false;
        let mut free: Vec<StorageRow> = vec![];
        let uploader_on = local_uploader_control_enabled();
        let busy = self.storage_busy.load(Ordering::Relaxed);
        let note = self.storage_note.lock().unwrap().clone();
        let quota = self.quota_txt.lock().unwrap().clone();
        let rows = self.storage_rows.lock().unwrap().clone();
        if rows.is_empty() && note.is_empty() && !busy {
            self.storage_scan();
        }
        egui::Window::new(
            egui::RichText::new("What takes storage").size(17.0).strong().color(INK),
        )
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .frame(
            egui::Frame::none()
                .fill(PAPER)
                .stroke(Stroke::new(1.5, INK))
                .rounding(egui::Rounding::same(10.0))
                .inner_margin(egui::Margin::same(16.0)),
        )
        .show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(egui::RichText::new(quota).size(13.0).color(INK));
            ui.add_space(8.0);
            scan = ui
                .add_enabled(
                    !busy,
                    egui::Button::new(if busy {
                        "checking…"
                    } else {
                        "check every photo and video"
                    }),
                )
                .clicked();
            if !note.is_empty() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(note).size(12.5).color(INK_SOFT));
            }
            if rows.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "Nothing in your library is charged for. Every photo and video was \
                         uploaded through the free endpoint, which keeps the original bytes \
                         and adds nothing to your storage. If the figure Photos shows is not \
                         zero, it belongs to items that are already deleted — Google keeps \
                         counting those until the 30-day window closes — not to anything here.",
                    )
                    .size(12.5)
                    .color(INK_SOFT),
                );
            } else {
                ui.add_space(8.0);
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for row in &rows {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(truncate(&row.filename, 34))
                                    .size(13.0)
                                    .color(INK),
                            );
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} · {}",
                                    date_label(row.ts),
                                    human_size(row.size)
                                ))
                                .size(12.0)
                                .color(INK_SOFT),
                            );
                            if ui
                                .add_enabled(uploader_on, egui::Button::new("make free").small())
                                .clicked()
                            {
                                free.push(row.clone());
                            }
                        });
                    }
                });
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "make free downloads the original, keeps its date, uploads it through \
                         the free endpoint and moves the charged copy to the trash. The bytes \
                         are identical, so nothing is lost.",
                    )
                    .size(12.0)
                    .color(INK_SOFT),
                );
                if ui
                    .add_enabled(uploader_on, egui::Button::new("make all of them free"))
                    .clicked()
                {
                    free = rows.clone();
                }
            }
        });
        self.storage_open = open;
        if scan {
            self.storage_scan();
        }
        if free.len() == 1 {
            self.reupload_free(free.into_iter().next().unwrap());
        } else {
            self.reupload_free_batch(free);
        }
    }

    fn storage_card(&mut self, ui: &mut egui::Ui) {
        let uploader_on = local_uploader_control_enabled();
        let busy = self.storage_busy.load(Ordering::Relaxed);
        let note = self.storage_note.lock().unwrap().clone();
        let n = self.storage_rows.lock().unwrap().len();
        let quota = self.quota_txt.lock().unwrap().clone();
        let mut engine = self.engine.clone();
        let (mut scan, mut show, mut export, mut open_dir) = (false, false, false, false);
        settings_card(ui, "Storage and quota", |ui| {
            ui.label(egui::RichText::new(quota).size(13.5).color(INK));
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Nothing you upload here takes storage: the free endpoint keeps the \
                     original bytes and adds nothing to your quota. This checks every photo \
                     and video in every list, including the locked folder and the trash, and \
                     lists anything Google does charge for. A charged item can be made free \
                     in place — same bytes, same date, no storage used.",
                )
                .size(13.0)
                .color(INK_SOFT),
            );
            ui.add_space(8.0);
            scan = ui
                .add_enabled(
                    !busy,
                    egui::Button::new(if busy {
                        "checking…"
                    } else {
                        "check what takes storage"
                    }),
                )
                .clicked();
            if !note.is_empty() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(note).size(12.5).color(INK_SOFT));
            }
            if n > 0 {
                ui.add_space(6.0);
                show = ui
                    .button(egui::RichText::new(format!("see the {} item(s)", n)).strong())
                    .clicked();
            }
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new("Where the work runs")
                    .size(14.5)
                    .color(INK),
            );
            ui.add_space(4.0);
            for (key, label, blurb) in [
                (
                    "local",
                    "This computer",
                    "Downloads the original and uploads it again right here. Nothing leaves \
                     the machine except to Google, and it is done in seconds per item.",
                ),
                (
                    "colab",
                    "The Colab account",
                    "Writes a job folder instead: a list of the items, a notebook and a \
                     README. You upload that folder to Drive, run the notebook on Colab and \
                     paste the cookie and the local URL list into the notebook — worth it \
                     for a whole library, pointless for two files.",
                ),
            ] {
                let on = engine == key;
                // Only the local choice spawns the uploader; keep Colab selectable
                // so a persisted "local" engine can still be switched away from.
                let enabled = uploader_on || key != "local";
                if ui
                    .add_enabled(
                        enabled,
                        egui::RadioButton::new(on, egui::RichText::new(label).size(14.0).color(INK)),
                    )
                    .clicked()
                {
                    engine = key.to_string();
                }
                ui.label(egui::RichText::new(blurb).size(12.5).color(INK_SOFT));
                ui.add_space(6.0);
            }
            if !uploader_on {
                ui.label(
                    egui::RichText::new(local_uploader_notice(false))
                        .size(12.0)
                        .color(INK_SOFT),
                );
                ui.add_space(6.0);
            }
            ui.horizontal(|ui| {
                export = ui
                    .add_enabled(
                        n > 0,
                        egui::Button::new("write a Colab job from this list"),
                    )
                    .clicked();
                open_dir = ui.button("open the job folder").clicked();
            });
            ui.label(
                egui::RichText::new(format!(
                    "job folder: {}",
                    preview_dir()
                        .parent()
                        .map(|p| p.join("colab-job").display().to_string())
                        .unwrap_or_default()
                ))
                .size(11.5)
                .color(INK_SOFT),
            );
        });
        if engine != self.engine {
            self.engine = engine;
            set_setting("engine", &self.engine);
        }
        if scan {
            self.storage_scan();
        }
        if show {
            self.storage_open = true;
        }
        if export {
            let rows = self.storage_rows.lock().unwrap().clone();
            if rows.is_empty() {
                let tx = self.tx.clone();
                let _ = tx.send(Msg::Toast {
                    msg: "nothing is charged, so there is no job to write".into(),
                    is_error: false,
                });
            } else {
                self.colab_export(&rows);
            }
        }
        if open_dir {
            let dir = preview_dir()
                .parent()
                .map(|p| p.join("colab-job"))
                .unwrap_or_else(|| std::env::temp_dir().join("gphotos-colab-job"));
            let _ = std::fs::create_dir_all(&dir);
            open_url(&dir.to_string_lossy());
        }
    }

    fn controls_card(&mut self, ui: &mut egui::Ui) {
        settings_card(ui, "Controls, playback and selection", |ui| {
            ui.label(
                egui::RichText::new("Selecting")
                    .size(14.5)
                    .color(INK),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Click a photo to open it. Ctrl/Cmd-click or the circle in its top-left corner selects it; shift-click takes everything from the last one you picked. The same works on album tiles.",
                )
                .size(12.5)
                .color(INK_SOFT),
            );
            ui.add_space(10.0);
            ui.label(egui::RichText::new("Player").size(14.5).color(INK));
            ui.add_space(4.0);
            let mut hide = self.ctl_hide_secs as f32;
            ui.add(
                egui::Slider::new(&mut hide, 0.5..=10.0)
                    .suffix(" s")
                    .text("hide controls after"),
            );
            if (hide as f64 - self.ctl_hide_secs).abs() > 0.01 {
                self.ctl_hide_secs = hide as f64;
                set_setting("ctl_hide", &format!("{:.1}", self.ctl_hide_secs));
            }
            let mut step = self.seek_step as f32;
            ui.add(egui::Slider::new(&mut step, 1.0..=120.0).suffix(" s").text("arrow key / J L seek"));
            if (step as f64 - self.seek_step).abs() > 0.01 {
                self.seek_step = step as f64;
                set_setting("seek_step", &format!("{:.0}", self.seek_step));
            }
            let mut slide = self.slideshow_secs as f32;
            ui.add(
                egui::Slider::new(&mut slide, 0.0..=30.0)
                    .suffix(" s")
                    .text("photo slideshow (0 = off)"),
            );
            if (slide as f64 - self.slideshow_secs).abs() > 0.01 {
                self.slideshow_secs = slide as f64;
                set_setting("slideshow", &format!("{:.0}", self.slideshow_secs));
            }
            ui.add_space(6.0);
            for (label, value, key, tip) in [
                (
                    "Start videos playing",
                    self.autoplay,
                    "autoplay",
                    "Off opens a video paused at the first frame.",
                ),
                (
                    "Loop videos by default",
                    self.loop_default,
                    "loop",
                    "The loop button in the player still overrides this for one film.",
                ),
                (
                    "Confirm before permanent delete",
                    self.confirm_del_setting,
                    "confirm_del",
                    "Leave this on unless you like living dangerously.",
                ),
                (
                    "Copy sharing links to the clipboard",
                    self.copy_links,
                    "copy_links",
                    "Off shows the link in a message instead of copying it.",
                ),
                (
                    "Storage summary in the sidebar",
                    self.show_storage,
                    "storage_sidebar",
                    "The measured totals you see on the main Photos page.",
                ),
            ] {
                let mut v = value;
                if ui
                    .checkbox(
                        &mut v,
                        egui::RichText::new(label).size(14.0).color(INK),
                    )
                    .changed()
                {
                    set_setting(key, if v { "1" } else { "0" });
                    match key {
                        "autoplay" => self.autoplay = v,
                        "loop" => self.loop_default = v,
                        "confirm_del" => self.confirm_del_setting = v,
                        "copy_links" => self.copy_links = v,
                        "storage_sidebar" => self.show_storage = v,
                        _ => {}
                    }
                }
                ui.label(egui::RichText::new(tip).size(12.0).color(INK_SOFT));
                ui.add_space(4.0);
            }
        });
    }

    fn look_card(&mut self, ui: &mut egui::Ui) {
        let mut size = self.thumb_size;
        settings_card(ui, "Look and arrangement", |ui| {
            ui.label(
                egui::RichText::new("Thumbnail size on the grid")
                    .size(14.5)
                    .color(INK),
            );
            ui.add_space(4.0);
            ui.add(
                egui::Slider::new(&mut size, 96.0..=220.0)
                    .show_value(false)
                    .text("size"),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "{} px squares — about {} fit across this window. Holding ctrl and \
                     turning the wheel over the grid does the same thing, and the size is \
                     remembered next time.",
                    size as i32,
                    (ui.available_width() / size).max(2.0) as i32,
                ))
                .size(13.0)
                .color(INK_SOFT),
            );
            ui.add_space(10.0);
            let mut sound = self.video_sound;
            if ui
                .checkbox(
                    &mut sound,
                    egui::RichText::new("Start videos with sound").size(14.5).color(INK),
                )
                .changed()
            {
                self.video_sound = sound;
                set_setting("video_sound", if sound { "1" } else { "0" });
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "On — a video plays audible as soon as it opens, which is what you want \
                     when you clicked it on purpose. Off — it opens muted and the speaker \
                     button turns the sound on.",
                )
                .size(13.0)
                .color(INK_SOFT),
            );
            ui.add_space(10.0);
            let mut resume = self.resume_ok;
            if ui
                .checkbox(
                    &mut resume,
                    egui::RichText::new("Pick videos up where I stopped").size(14.5).color(INK),
                )
                .changed()
            {
                self.resume_ok = resume;
                set_setting("resume", if resume { "1" } else { "0" });
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "A film you did not finish opens at the second you left, with a small \
                     \"start over\" next to it. Anything within ten seconds of the end is \
                     treated as finished and starts from the top.",
                )
                .size(13.0)
                .color(INK_SOFT),
            );
            ui.add_space(10.0);
            let mut rail = self.rail_on;
            if ui
                .checkbox(
                    &mut rail,
                    egui::RichText::new("Month rail on the right of the timeline")
                        .size(14.5)
                        .color(INK),
                )
                .changed()
            {
                self.rail_on = rail;
                set_setting("rail", if rail { "1" } else { "0" });
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "A thin strip whose segments are as tall as that month is full. Hover to \
                     see \"Jul 2019 · 231\", click or drag to go there.",
                )
                .size(13.0)
                .color(INK_SOFT),
            );
        });
        if (size - self.thumb_size).abs() > 0.01 {
            self.thumb_size = size;
            set_setting("thumb_size", &format!("{}", size as i32));
        }
    }

    fn upload_card(&mut self, ui: &mut egui::Ui) {
        let uploader_on = local_uploader_control_enabled();
        let mut mode = self.upload_mode.clone();
        let mut pick = false;
        settings_card(ui, "Uploads", |ui| {
            ui.label(
                egui::RichText::new("What an upload costs you")
                    .size(14.5)
                    .color(INK),
            );
            ui.add_space(6.0);
            for (key, label, blurb) in [
                (
                    "free",
                    "No storage used, original quality",
                    "Uploads through the phone-style endpoint, so the file keeps its original \
                     bytes and does not count against your Google storage. This is what the \
                     library already uses for everything in it.",
                ),
                (
                    "saver",
                    "Storage saver, smaller file",
                    "Google recompresses the upload so it takes less room, and it still counts \
                     against your storage. Quality is lower, so this is only worth it if you \
                     are short of space.",
                ),
                (
                    "quota",
                    "Original quality, counts against storage",
                    "The plain web upload: exact bytes, and it uses your Google storage like \
                     any other upload from a browser.",
                ),
            ] {
                let on = mode == key;
                if ui
                    .radio(on, egui::RichText::new(label).size(14.0).color(INK))
                    .clicked()
                {
                    mode = key.to_string();
                }
                ui.label(
                    egui::RichText::new(blurb)
                        .size(12.5)
                        .color(INK_SOFT),
                );
                ui.add_space(8.0);
            }
            ui.add_space(2.0);
            pick = ui
                .add_enabled(
                    uploader_on,
                    egui::Button::new(
                        egui::RichText::new("upload photos or videos…").size(14.5).strong(),
                    ),
                )
                .clicked();
            if !uploader_on {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(local_uploader_notice(false))
                        .size(12.0)
                        .color(INK_SOFT),
                );
            }
        });
        if mode != self.upload_mode {
            self.upload_mode = mode;
            set_setting("upload_mode", &self.upload_mode);
        }
        if pick {
            self.upload_pick();
        }
    }

    // stage chosen files in the review popup — nothing uploads until Start
    fn upload_pick(&mut self) {
        let files = picked_files(
            rfd::FileDialog::new()
                .set_title("Choose photos or videos to upload")
                .pick_files(),
        );
        if !files.is_empty() {
            self.upload_queue.extend(files.into_iter().map(std::path::PathBuf::from));
            self.upload_popup = true;
        }
    }

    // the popup: file list, mode, and an explicit Start button
    fn upload_popup_ui(&mut self, ctx: &egui::Context) {
        if !self.upload_popup {
            return;
        }
        let mut open = true;
        let mut start = false;
        let mut cancel = false;
        egui::Window::new("Upload files")
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(
                    "These files are only staged. Nothing is uploaded until you press Start."
                ).size(13.0).color(INK));
                ui.add_space(6.0);
                if self.upload_queue.is_empty() {
                    ui.label("No files selected.");
                } else {
                    egui::Grid::new("upload_queue_grid").num_columns(2).show(ui, |ui| {
                        for (i, p) in self.upload_queue.iter().enumerate() {
                            let name = p.file_name().map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| p.to_string_lossy().into_owned());
                            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                            ui.label(egui::RichText::new(truncate(&name, 48)).size(12.5));
                            ui.label(egui::RichText::new(human_size(size)).size(11.5).color(INK_SOFT));
                            ui.end_row();
                            let _ = i;
                        }
                    });
                }
                ui.add_space(8.0);
                let cost = match self.upload_mode.as_str() {
                    "saver" => "storage saver (smaller file, uses quota)",
                    "quota" => "original quality, uses quota",
                    _ => "free endpoint (no storage used, original quality)",
                };
                ui.label(egui::RichText::new(format!("Upload mode: {cost}")).size(12.5));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    start = ui.add_enabled(
                        !self.upload_queue.is_empty(),
                        egui::Button::new(egui::RichText::new("Start upload").size(14.0).strong())
                    ).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if cancel {
            self.upload_queue.clear();
            self.upload_popup = false;
        }
        if !open {
            self.upload_popup = false;
        }
        if start && !self.upload_queue.is_empty() {
            self.start_upload();
            self.upload_queue.clear();
            self.upload_popup = false;
        }
    }

    // actually run the uploader on the staged files
    fn start_upload(&self) {
        let files = self.upload_queue.clone();
        let mode = self.upload_mode.clone();
        let bin = std::env::var("GPHOTOS_GOTOHP")
            .unwrap_or_else(|_| gotohp_bin().to_string_lossy().into_owned());
        self.spawn_action(move |tx| {
            let mut c = Command::new(&bin);
            c.arg("upload").args(&files).arg("--no-tui").arg("-l").arg("warn");
            match mode.as_str() {
                "saver" => { c.arg("--saver"); }
                "quota" => { c.arg("--use-quota"); }
                _ => {}
            }
            let (ok, out) = run_cmd(&mut c);
            let msg = if ok {
                format!("uploaded {} file(s) · {}", files.len(), match mode.as_str() {
                    "saver" => "storage saver", "quota" => "counts against storage",
                    _ => "no storage used",
                })
            } else {
                format!("upload failed: {}", truncate(out.lines().last().unwrap_or("uploader said no"), 120))
            };
            let _ = tx.send(Msg::Toast { msg, is_error: !ok });
        });
    }

    fn cache_card(&mut self, ui: &mut egui::Ui) {
        let mut on = self.cache_on;
        let mut changed = false;
        settings_card(ui, "Caching on this computer", |ui| {
            changed = ui
                .checkbox(
                    &mut on,
                    egui::RichText::new("Keep photos, videos and thumbnails here so they open fast")
                        .size(14.5)
                        .color(INK),
                )
                .changed();
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "On — everything you open is downloaded once and kept on this computer. \
                     The next open is instant, and seeking inside a video reads a local file \
                     instead of asking Google for the stream again, which is what makes seeks \
                     feel slow. Nothing is uploaded, nothing is removed from your library; the \
                     only cost is disk space, shown below.",
                )
                .size(13.0)
                .color(INK_SOFT),
            );
            ui.add_space(5.0);
            ui.label(
                egui::RichText::new(
                    "Off — every photo and video is fetched from Google each time. Opening is \
                     slower, seeking waits on the network, and no disk space is used. \
                     Thumbnails stay in memory only while the app is running.",
                )
                .size(13.0)
                .color(INK_SOFT),
            );
            ui.add_space(10.0);
            if self.cache_sizes.is_none() {
                self.cache_sizes = Some((
                    dir_size(&thumb_cache_dir()),
                    dir_size(&full_cache_dir()),
                    dir_size(&video_cache_dir()),
                ));
            }
            if let Some((thumbs, full, vids)) = self.cache_sizes {
                ui.label(
                    egui::RichText::new(format!(
                        "thumbnails {}   ·   photos {}   ·   videos {}   ·   total {}",
                        human_gb(thumbs),
                        human_gb(full),
                        human_gb(vids),
                        human_gb(thumbs + full + vids)
                    ))
                    .size(15.0)
                    .strong()
                    .color(INK),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("refresh usage").clicked() {
                        self.cache_sizes = None;
                    }
                    if ui.button("clear cache").clicked() {
                        let freed = thumbs + full + vids;
                        clear_caches();
                        self.cache_sizes = None;
                        // recompute from disk: an in-flight .part may still be
                        // held open by curl, so the pre-clear snapshot lies
                        let actual = dir_size(&thumb_cache_dir())
                            + dir_size(&full_cache_dir())
                            + dir_size(&video_cache_dir());
                        let _ = self.tx.send(Msg::Toast {
                            msg: if actual == 0 {
                                format!("cache cleared, {} freed", human_gb(freed))
                            } else {
                                format!("cache cleared, {} freed ({} still in use)", human_gb(freed), human_gb(actual))
                            },
                            is_error: false,
                        });
                    }
                });
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!(
                        "{}  ·  {}  ·  {}",
                        thumb_cache_dir().display(),
                        full_cache_dir().display(),
                        video_cache_dir().display()
                    ))
                    .size(11.0)
                    .color(INK_SOFT),
                );
            }
        });
        if changed {
            self.cache_on = on;
            save_cache_flag(on);
            if !on {
                clear_caches();
            }
            self.cache_sizes = None;
        }
    }

    fn locked_card(&mut self, ui: &mut egui::Ui) {
        let mut show = self.locked_in_ui;
        let mut changed = false;
        let mut open = false;
        settings_card(ui, "Locked folder", |ui| {
            changed = ui
                .checkbox(
                    &mut show,
                    egui::RichText::new("Show Locked folder in the sidebar").size(14.5).color(INK),
                )
                .changed();
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "The locked folder holds items you moved away from the normal library. \
                     Turning this off only hides the shortcut here — nothing is unlocked, \
                     moved or deleted, and the folder stays exactly as it is in Google Photos.",
                )
                .size(13.0)
                .color(INK_SOFT),
            );
            ui.add_space(10.0);
            open = ui
                .button(egui::RichText::new("open locked folder").size(14.5).strong())
                .clicked();
        });
        if changed {
            self.locked_in_ui = show;
            set_setting("locked_in_ui", if show { "1" } else { "0" });
        }
        if open {
            *self.page.lock().unwrap() = "photos".into();
            self.fetch_lib("locked".into());
        }
    }

    fn sync_page(&mut self, ctx: &egui::Context) {
        if !linux_sync_available() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    if ui.button("< back to photos").clicked() {
                        *self.page.lock().unwrap() = "photos".into();
                    }
                    ui.add_space(40.0);
                    ui.label(egui::RichText::new("Sync robot").size(28.0).strong());
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new(linux_sync_notice(false)).color(INK_SOFT));
                });
            });
            return;
        }
        let (watcher, account, uploads_n, retries, db_mb, unit_ok, today_n) = {
            let s = self.snap.lock().unwrap();
            (
                s.watcher,
                s.account.clone(),
                s.total_uploads,
                s.retries,
                s.db_mb,
                s.unit_installed,
                s.today_uploads,
            )
        };

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            if ui.button("< back to photos").clicked() {
                *self.page.lock().unwrap() = "photos".into();
            }
            let t = self.start.elapsed().as_secs_f32();
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(78.0, 84.0), egui::Sense::hover());
                let painter = ui.painter_at(rect);
                draw_logo(&painter, rect.left_top(), 1.0, t);
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    let title = ui.label(egui::RichText::new("Google Photos").size(30.0).strong());
                    let tr = title.rect;
                    sketchy_line_wavy(
                        ui.painter(),
                        Pos2::new(tr.left() + 1.0, tr.bottom() + 2.0),
                        Pos2::new(tr.left() + tr.width() * 0.62, tr.bottom() + 3.0),
                        RED,
                        3.0,
                        12345,
                        t,
                    );
                    ui.label(
                        egui::RichText::new("your photos, synced like magic (no magic, just a robot)")
                            .color(INK_SOFT)
                            .size(14.0),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (txt, col) = if watcher {
                        ("watcher running", GREEN)
                    } else {
                        ("watcher asleep", INK_SOFT)
                    };
                    ui.colored_label(col, txt);
                    if account.is_empty() {
                        ui.colored_label(RED, "not signed in");
                    } else {
                        ui.label(format!("account: {account}"));
                    }
                });
            });
            ui.add_space(4.0);
            // sketchy page divider — short, quiet, only under the header text
            let r = ui.max_rect();
            sketchy_line(
                ui.painter(),
                Pos2::new(r.left() + 16.0, r.bottom() + 3.0),
                Pos2::new(r.left() + 320.0, r.bottom() + 2.0),
                INK_SOFT,
                1.5,
                7,
            );
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (t, v, col) in [
                    ("Uploaded", uploads_n.to_string(), BLUE),
                    ("Retry queue", retries.to_string(), if retries > 0 { YELLOW } else { INK_SOFT }),
                    ("Tracked DB", format!("{db_mb:.2} MB"), INK_SOFT),
                ] {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(t).size(13.0).color(INK_SOFT));
                        ui.label(egui::RichText::new(v).size(22.0).strong().color(col));
                    });
                    ui.separator();
                }
                let cap_txt = {
                    let c = self.cap.trim().to_string();
                    let cn: u64 = c.parse().unwrap_or(0);
                    if cn > 0 { format!("{} / {}", today_n, cn) } else { today_n.to_string() }
                };
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Uploaded today").size(13.0).color(INK_SOFT));
                    ui.label(egui::RichText::new(cap_txt).size(22.0).strong().color(GREEN));
                });
            });
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                let free = !self.busy.load(Ordering::Relaxed);
                if ui.add_enabled(free && !watcher, egui::Button::new("Start watcher")).clicked() {
                    self.watcher_action("start");
                }
                if ui.add_enabled(free && watcher, egui::Button::new("Stop")).clicked() {
                    self.watcher_action("stop");
                }
                if ui
                    .add_enabled(
                        free,
                        egui::Button::new(if unit_ok { "Autostart off" } else { "Autostart on" }),
                    )
                    .clicked()
                {
                    self.watcher_action(if unit_ok { "autostart_off" } else { "autostart_on" });
                }
                if ui.add_enabled(free, egui::Button::new("Rescan now")).clicked() {
                    self.rescan();
                }
                if ui.add_enabled(free, egui::Button::new("Check for updates")).clicked() {
                    self.update_app();
                }
            });

            ui.add_space(6.0);
            sketchy_heading(ui, "Folders being synced");
            ui.add_space(2.0);

            // folder chips with little x buttons
            let dirs: Vec<String> = self
                .dirs_text
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let mut removed: Option<usize> = None;
            ui.horizontal_wrapped(|ui| {
                if dirs.is_empty() {
                    ui.label(egui::RichText::new("nothing yet — add a folder below")
                        .color(INK_SOFT));
                }
                for (i, d) in dirs.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let chip = egui::Button::new(format!(" {} ", truncate(d, 44)));
                        if ui.add(chip).clicked() {
                            open_url(d);
                        }
                        if ui.small_button("x").clicked() {
                            removed = Some(i);
                        }
                    });
                }
            });
            if let Some(i) = removed {
                let mut remain = dirs.clone();
                remain.remove(i);
                self.dirs_text = remain.join("\n");
                let t = self.dirs_text.clone();
                self.save_dirs(t);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !self.busy.load(Ordering::Relaxed),
                        egui::Button::new("+ Add folder…"),
                    )
                    .clicked()
                {
                    self.pick_folder();
                }
                if !dirs.is_empty()
                    && ui
                        .add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new("Save"))
                        .clicked()
                {
                    let t = self.dirs_text.clone();
                    self.save_dirs(t);
                }
                ui.label(
                    egui::RichText::new("click a folder chip to peek inside")
                        .color(INK_SOFT)
                        .size(12.0),
                );
            });

            if account.is_empty() {
                ui.add_space(6.0);
                sketchy_heading(ui, "Sign in to Google Photos");
                if ui
                    .add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new("Open browser & sign me in automatically"))
                    .clicked()
                {
                    self.signin_auto();
                }
                ui.label(egui::RichText::new("opens your browser — the page WILL ask for your password once (it's Google's device-setup flow, it never trusts an existing login). Type it once: the token is captured by itself, and this machine's Google session revives too — library included.").size(12.0).color(INK_SOFT));
                ui.collapsing("Manual token paste — sync-robot credential backup only", |ui| {
                    ui.label("Normal accounts: use add/create account in the accounts window — no token needed.");
                    ui.label("1. Open the sign-in page (it will spin forever after I Agree — that is normal)");
                    ui.hyperlink("https://accounts.google.com/embedded/setup/v2/android");
                    ui.label("2. F12 -> Application/Storage -> Cookies -> accounts.google.com");
                    ui.label("3. Copy the oauth_token cookie and paste it here:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token)
                            .password(true)
                            .hint_text("oauth_token cookie value")
                            .desired_width(420.0),
                    );
                    if ui
                        .add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new("Connect account"))
                        .clicked()
                    {
                        let t = self.token.clone();
                        self.token.clear();
                        self.add_creds(t);
                    }
                });
                ui.add_space(6.0);
            }

            ui.add_space(8.0);
            sketchy_heading(ui, "Speed & limits");
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("pause between uploads (seconds, 0 = off)").size(12.0).color(INK_SOFT));
                    ui.add(egui::TextEdit::singleline(&mut self.delay_s).desired_width(90.0));
                });
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("daily cap (files per day, 0 = unlimited)").size(12.0).color(INK_SOFT));
                    ui.add(egui::TextEdit::singleline(&mut self.cap).desired_width(90.0));
                });
            });
            ui.collapsing("Immich tagging (optional, needs an Immich server)", |ui| {
                ui.label(egui::RichText::new("after each Google Photos upload, the matching asset in your Immich server gets a tag").size(12.0).color(INK_SOFT));
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label("server url");
                        ui.add(egui::TextEdit::singleline(&mut self.immich_url).hint_text("http://localhost:2283").desired_width(240.0));
                    });
                    ui.vertical(|ui| {
                        ui.label("api key");
                        ui.add(egui::TextEdit::singleline(&mut self.immich_key).password(true).desired_width(200.0));
                    });
                    ui.vertical(|ui| {
                        ui.label("tag name");
                        ui.add(egui::TextEdit::singleline(&mut self.immich_tag).desired_width(150.0));
                    });
                });
            });
            ui.add_space(8.0);
            sketchy_heading(ui, "Upload extras");
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("album — every upload lands in this Google album (created if missing)").size(12.0).color(INK_SOFT));
                    ui.add(egui::TextEdit::singleline(&mut self.album).hint_text("e.g. Phone backups").desired_width(260.0));
                });
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("free up space").size(12.0).color(INK_SOFT));
                    ui.checkbox(&mut self.delete_after, "delete the local file after it uploads");
                });
            });
            if ui
                .add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new("Save settings"))
                .clicked()
            {
                let (d, c, u, k, t, al, da) = (
                    self.delay_s.clone(),
                    self.cap.clone(),
                    self.immich_url.clone(),
                    self.immich_key.clone(),
                    self.immich_tag.clone(),
                    self.album.clone(),
                    self.delete_after,
                );
                self.save_settings(d, c, u, k, t, al, da);
            }

            ui.add_space(8.0);
            sketchy_heading(ui, "Accounts");
            if ui.add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new("Refresh list")).clicked() {
                self.refresh_accounts();
            }
            {
                let list = self.accounts.lock().unwrap().clone();
                if list.is_empty() {
                    ui.label(egui::RichText::new("press Refresh to peek at the accounts book").color(INK_SOFT));
                }
                for line in &list {
                    let active = line.starts_with('*');
                    let email = line.trim_start_matches('*').trim().to_string();
                    ui.horizontal(|ui| {
                        if active {
                            ui.colored_label(GREEN, "in use");
                        }
                        ui.label(&email);
                        if !active && ui.add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new("switch to")).clicked() {
                            let em = email.clone();
                            self.spawn_action(move |tx| {
                                let (ok, msg) = run_cmd(Command::new(gotohp_bin()).arg("creds").arg("set").arg(&em));
                                let _ = tx.send(Msg::Toast {
                                    msg: if ok { format!("switched to {em}") } else { msg },
                                    is_error: !ok,
                                });
                            });
                        }
                        if ui.add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new("forget")).clicked() {
                            let em = email.clone();
                            self.spawn_action(move |tx| {
                                let (ok, msg) = run_cmd(Command::new(gotohp_bin()).arg("creds").arg("remove").arg(&em));
                                let _ = tx.send(Msg::Toast {
                                    msg: if ok { format!("forgot {em}") } else { msg },
                                    is_error: !ok,
                                });
                            });
                        }
                    });
                }
            }

            ui.add_space(8.0);
            sketchy_heading(ui, "The story so far");
            ui.columns(2, |cols| {
                cols[0].label("Recent uploads");
                egui::ScrollArea::vertical().id_salt("uploads").show(&mut cols[0], |ui| {
                    let rows = self.snap.lock().unwrap().uploads.clone();
                    if rows.is_empty() {
                        ui.label("Nothing uploaded yet");
                    }
                    for (t, name, size) in &rows {
                        ui.monospace(format!("{t}  {}  {}", truncate(name, 48), human_size(*size)));
                    }
                });
                cols[1].label("Activity log");
                egui::ScrollArea::vertical().id_salt("logs").show(&mut cols[1], |ui| {
                    let rows = self.snap.lock().unwrap().logs.clone();
                    if rows.is_empty() {
                        ui.label("No activity yet");
                    }
                    for (t, level, msg) in &rows {
                        let col = match level.as_str() {
                            "ERROR" => RED,
                            "WARN" => YELLOW,
                            _ => INK_SOFT,
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.monospace(format!("{t} [{level}]"));
                            ui.colored_label(col, truncate(msg, 84));
                        });
                    }
                });
            });

                });
        });
    }

    // sign-in-first page, like the real app's welcome gate
    fn signin_page(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Center), |ui| {
                ui.add_space(60.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(150.0, 160.0), egui::Sense::hover());
                let painter = ui.painter_at(rect);
                let t = self.start.elapsed().as_secs_f32();
                draw_logo(&painter, rect.left_top(), 2.0, t);
                ui.add_space(20.0);
                let title = ui.label(egui::RichText::new("Google Photos").size(38.0).strong());
                let tr = title.rect;
                sketchy_line_wavy(
                    ui.painter(),
                    Pos2::new(tr.left() + 2.0, tr.bottom() + 3.0),
                    Pos2::new(tr.left() + tr.width() * 0.8, tr.bottom() + 4.0),
                    RED,
                    3.5,
                    12345,
                    t,
                );
                ui.label(egui::RichText::new("your whole Google Photos, as a desktop app — plus the sync robot").size(15.0).color(INK_SOFT));
                ui.add_space(26.0);
                ui.horizontal(|ui| { ui.add_space(ui.available_width() / 2.0 - 150.0); });
                ui.label(egui::RichText::new("sign in to open your library").size(18.0).strong());
                if linux_sync_available() {
                    if ui.add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new(egui::RichText::new("Open browser & sign me in automatically").size(17.0))).clicked() {
                        self.signin_auto();
                    }
                    ui.label(egui::RichText::new("the page WILL ask for your password once (Google's device-setup flow never trusts an existing login). Type it once: the token is captured by itself and this machine's Google session revives too — library included.").size(12.0).color(INK_SOFT));
                }
                if ui.button("Add or create an account (detect sign-in automatically)").clicked() {
                    self.open_switch();
                }
                ui.add_space(8.0);
                if linux_sync_available() {
                ui.collapsing("Manual token paste — sync-robot credential backup only", |ui| {
                    ui.label("Normal accounts: use add/create account in the accounts window — no token needed.");
                    ui.label("1. Open the sign-in page (it will spin forever after I Agree — that is normal)");
                    ui.hyperlink("https://accounts.google.com/embedded/setup/v2/android");
                    ui.label("2. F12 -> Application/Storage -> Cookies -> accounts.google.com");
                    ui.label("3. Copy the oauth_token cookie and paste it here:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token)
                            .password(true)
                            .hint_text("oauth_token cookie value")
                            .desired_width(420.0),
                    );
                    if ui
                        .add_enabled(!self.busy.load(Ordering::Relaxed), egui::Button::new("Connect account"))
                        .clicked()
                    {
                        let t = self.token.clone();
                        self.token.clear();
                        self.add_creds(t);
                        // once connected, re-check the session and move on
                        let page = self.page.clone();
                        let sess = self.lib_session.clone();
                        let tx = self.tx.clone();
                        let sel = self.selected.clone();
                        let epoch_sh = self.account_epoch.clone();
                        let epoch = epoch_sh.load(Ordering::Relaxed);
                        thread::spawn(move || {
                            thread::sleep(Duration::from_secs(3));
                            let s = capture_selected(&sel);
                            if epoch_sh.load(Ordering::Relaxed) != epoch {
                                return;
                            }
                            let ok = s.is_some();
                            *sess.lock().unwrap() = s;
                            let _ = tx.send(Msg::SessionReady(ok));
                        });
                    }
                });
                } else {
                    ui.label(egui::RichText::new(linux_sync_notice(false)).size(12.0).color(INK_SOFT));
                }
                ui.add_space(30.0);
                if ui.button("advanced — sync robot, folders, settings (no sign-in needed)").clicked() {
                    *self.page.lock().unwrap() = "sync".into();
                }
            });
        });
    }

    // the real app: google-photos-like shell
    fn photos_page(&mut self, ctx: &egui::Context) {
        let t = self.start.elapsed().as_secs_f32();
        let account = self.snap.lock().unwrap().account.clone();

        // top bar
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(46.0, 50.0), egui::Sense::hover());
                let painter = ui.painter_at(rect);
                draw_logo(&painter, rect.left_top(), 0.6, t);
                let title = ui.label(egui::RichText::new("Photos").size(24.0).strong());
                let tr = title.rect;
                sketchy_line_wavy(
                    ui.painter(),
                    Pos2::new(tr.left() + 1.0, tr.bottom() + 2.0),
                    Pos2::new(tr.left() + tr.width() * 0.6, tr.bottom() + 3.0),
                    RED,
                    2.5,
                    12345,
                    t,
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // account chip: which account this is, click to switch
                    let email = self
                        .lib_session
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|s| s.email.clone())
                        .unwrap_or_default();
                    let chip = if email.is_empty() { "account".to_string() } else { truncate(&email, 30) };
                    if ui.button(chip).clicked() {
                        self.open_switch();
                    }
                    if linux_sync_available() {
                        ui.label(egui::RichText::new(if account.is_empty() { "sync robot asleep" } else { "sync robot ready" }).size(12.0).color(INK_SOFT));
                    }
                    ui.add_space(14.0);
                    let search_box = ui.add(
                        egui::TextEdit::singleline(&mut self.lib_search)
                            .hint_text("search photos, people, places… · file:name for local")
                            .desired_width(300.0),
                    );
                    let enter = ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                    if enter && search_box.has_focus() && !self.lib_search.trim().is_empty() {
                        let q = self.lib_search.clone();
                        self.run_search(&q);
                    }
                    if ui.button("Search").clicked() && !self.lib_search.trim().is_empty() {
                        let q = self.lib_search.clone();
                        self.run_search(&q);
                    }
                });
            });
            ui.add_space(2.0);
        });

        // left nav — google-photos-like
        egui::SidePanel::left("nav").resizable(false).show(ctx, |ui| {
            ui.add_space(8.0);
            let cur = self.current_lib_view();
            for (label, view) in [
                ("Photos", "recent"),
                ("Favorites", "favorites"),
                ("Archive", "archive"),
                ("Albums", "albums"),
                ("Shared albums", "shared"),
                ("Shared links", "links"),
                ("Things & places", "things"),
                ("Locked folder", "locked"),
                ("Trash", "trash"),
            ] {
                if view == "locked" && !self.locked_in_ui {
                    continue;
                }
                let active = cur == view
                    || (view == "links" && cur.starts_with("link:"))
                    || (view == "recent" && (cur.starts_with("search:") || cur.starts_with("album:")));
                let row = Rect::from_min_size(
                    ui.cursor().min,
                    Vec2::new(ui.available_width(), 27.0),
                );
                let resp = ui.allocate_rect(row, egui::Sense::click());
                if active {
                    ui.painter().rect_filled(row, 7.0, PAPER_DARK);
                    ui.painter().rect_filled(
                        Rect::from_min_size(
                            Pos2::new(row.left(), row.top() + 5.0),
                            Vec2::new(3.0, row.height() - 10.0),
                        ),
                        2.0,
                        YELLOW,
                    );
                } else if resp.hovered() {
                    ui.painter()
                        .rect_filled(row, 7.0, Color32::from_black_alpha(10));
                }
                ui.painter().text(
                    Pos2::new(row.left() + 14.0, row.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(14.5),
                    if active { INK } else { INK_SOFT },
                );
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() && !self.lib_busy.load(Ordering::Relaxed) {
                    self.fetch_lib(view.into());
                }
                ui.add_space(2.0);
            }
            ui.separator();
            if ui.button("Sync robot").clicked() {
                *self.page.lock().unwrap() = "sync".into();
            }
            if ui.button("Settings").clicked() {
                self.cache_sizes = None;
                *self.page.lock().unwrap() = "settings".into();
            }
            ui.add_space(6.0);
            // quota line with refresh right beside it, as requested
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(self.quota_txt.lock().unwrap().clone()).size(11.5).color(INK));
                if ui.small_button("refresh").clicked() {
                    self.fetch_quota();
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("Select all").clicked() {
                    self.select_all();
                }
                ui.label(
                    egui::RichText::new("ctrl+click to select · click to open · ctrl+a all · esc clear")
                        .size(10.0)
                        .color(INK_SOFT),
                );
            });
            ui.horizontal_wrapped(|ui| {
                // structured local filters — no RPC, they reuse the one-time
                // scan's data (or the loaded grid before that scan completes)
                // toggles: the ● marker and red text show which bits are on
                let vb = self.filter_bits & FILTER_VIDEOS != 0;
                if ui.small_button(egui::RichText::new(if vb { "● videos only" } else { "videos only" })
                        .color(if vb { RED } else { INK })).clicked() {
                    self.filter_bits = if vb { self.filter_bits & !FILTER_VIDEOS }
                        else { (self.filter_bits & !FILTER_PHOTOS) | FILTER_VIDEOS };
                    self.apply_local_filters();
                }
                let pb = self.filter_bits & FILTER_PHOTOS != 0;
                if ui.small_button(egui::RichText::new(if pb { "● photos only" } else { "photos only" })
                        .color(if pb { RED } else { INK })).clicked() {
                    self.filter_bits = if pb { self.filter_bits & !FILTER_PHOTOS }
                        else { (self.filter_bits & !FILTER_VIDEOS) | FILTER_PHOTOS };
                    self.apply_local_filters();
                }
                if ui.small_button("bursts").clicked() {
                    self.show_bursts();
                }
                let scanned = !self.all_items.lock().unwrap().is_empty();
                // sizes/years arrive with the scan, so hide these until it is done
                if scanned && !self.sizes.lock().unwrap().is_empty() {
                    let lb = self.filter_bits & FILTER_LARGE != 0;
                    if ui.small_button(egui::RichText::new(if lb { "● large files" } else { "large files" })
                            .color(if lb { RED } else { INK })).clicked() {
                        self.filter_bits ^= FILTER_LARGE;
                        self.apply_local_filters();
                    }
                }
                if scanned {
                    let mut top: Vec<(i64, usize)> = if let Some(st) = &self.hog_stats {
                        st.years.iter().map(|(y, n, _)| (*y, *n)).collect()
                    } else {
                        // stats not posted yet but all_items is: derive locally
                        let mut m: std::collections::BTreeMap<i64, usize> = Default::default();
                        for i in self.all_items.lock().unwrap().iter() {
                            if i.ts > 0 {
                                *m.entry(library::civil_from_days(i.ts.div_euclid(86400)).0)
                                    .or_default() += 1;
                            }
                        }
                        m.into_iter().collect()
                    };
                    top.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0))); // most items first
                    top.truncate(4);
                    top.sort_by(|a, b| b.0.cmp(&a.0)); // newest first in the chip row
                    for (y, _) in top {
                        if ui.small_button(y.to_string()).clicked() {
                            self.local_subset(&format!("year:{}", y), true, |i| {
                                i.ts > 0
                                    && library::civil_from_days(i.ts.div_euclid(86400)).0 == y
                            });
                        }
                    }
                }
                // server-side searches (these do hit Google)
                for (label, q) in [
                    ("screenshots", "screenshots"),
                    ("monochrome", "monochrome"),
                    ("2015 to 2017", "2015 to 2017"),
                ] {
                    if ui.small_button(label).clicked() {
                        self.lib_search = q.into();
                        self.fetch_lib(format!("search:{}", q));
                    }
                }
            });
            if ui.small_button("biggest items").clicked() {
                self.fetch_hogs();
            }
            if ui.small_button("duplicate candidates").clicked() {
                self.fetch_lib("dupes".into());
            }
            if ui.small_button("timeline chart").clicked() {
                if std::env::var("GPHOTOS_CTXDBG").is_ok() {
                    eprintln!("[CTX] timeline chart clicked");
                }
                *self.lib_view.lock().unwrap() = "timeline".into();
                *self.lib_items.lock().unwrap() = vec![];
            }
            ui.add_space(8.0);
            if !self.show_storage {
                return;
            }
            ui.label(egui::RichText::new("your storage, measured").size(11.5).strong().color(INK));
            if let Some(st) = &self.hog_stats {
                let lines = library::stats_lines(st);
                for want in [
                    "measured",
                    "together they weigh",
                    "charged to your quota",
                ] {
                    if let Some((_, v)) = lines.iter().find(|(k, _)| k == want) {
                        ui.label(
                            egui::RichText::new(wrap_line(v, 38))
                                .size(10.5)
                                .color(INK_SOFT),
                        );
                        ui.add_space(2.0);
                    }
                }
            } else {
                ui.label(
                    egui::RichText::new(format!(
                        "measuring every list… ({} loaded)",
                        self.lib_items.lock().unwrap().len()
                    ))
                    .size(10.5)
                    .color(INK_SOFT),
                );
            }

        });

        // the month rail: a pinned strip whose segments are sized by how many
        // items each month holds
        if !(self.rail_on && self.current_lib_view() == "recent" && self.viewer.is_none()) {
            self.grid_drag = false;
        }
        if self.rail_on && self.current_lib_view() == "recent" && self.viewer.is_none() {
            let buckets = self.month_buckets();
            if buckets.len() > 1 {
                egui::SidePanel::right("month_rail")
                    .resizable(false)
                    .frame(egui::Frame::none())
                    .exact_width(34.0)
                    .show(ctx, |ui| {
                        self.timeline_rail(ui, &buckets);
                    });
            }
        }

        // content
        egui::CentralPanel::default().show(ctx, |ui| {
            // first-run hints: inline, so the grid stays usable; global setting
            // keeps this to one appearance per install (not per account)
            if self.current_lib_view() == "recent" && setting("onboarded", "0") != "1" {
                egui::Frame::none()
                    .fill(PAPER)
                    .stroke(Stroke::new(1.5, INK))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(16.0))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("A few things worth knowing")
                                .size(15.0)
                                .strong()
                                .color(INK),
                        );
                        ui.add_space(6.0);
                        for (key, what) in [
                            ("ctrl / shift + click", "select one tile, or a range"),
                            ("?", "open the keyboard shortcuts card"),
                            ("g", "jump to a date"),
                        ] {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(key)
                                        .size(12.5)
                                        .strong()
                                        .color(INK)
                                        .background_color(YELLOW.gamma_multiply(0.45)),
                                );
                                ui.label(egui::RichText::new(what).size(12.5).color(INK_SOFT));
                            });
                        }
                        ui.add_space(8.0);
                        if ui.button("got it").clicked() {
                            set_setting("onboarded", "1");
                        }
                    });
                ui.add_space(10.0);
            }
            // selection action strip (google shows one too, when you select)
            let n = self.lib_selected.lock().unwrap().len();
            if n > 0 {
                let sv = self.current_lib_view();
                let s_trash = sv == "trash";
                let s_archive = sv == "archive";
                let s_locked = sv == "locked";
                let s_shared = sv.starts_with("link:");
                let s_favs = sv == "favorites";
                ui.horizontal_wrapped(|ui| {
                    // show what the selection weighs and how much video it holds
                    let sel = self.lib_selected.lock().unwrap();
                    let all = self.lib_items.lock().unwrap();
                    let sz = self.sizes.lock().unwrap();
                    let mut bytes = 0u64;
                    let (mut vids, mut ms) = (0usize, 0u64);
                    for k in sel.iter() {
                        bytes += sz.get(k).copied().unwrap_or(0);
                        if let Some(it) = all.iter().find(|i| &i.media_key == k) {
                            if it.is_video {
                                vids += 1;
                                ms += it.duration_ms;
                            }
                        }
                    }
                    let mut label = format!("{} selected", n);
                    if bytes > 0 {
                        label.push_str(&format!(" · {}", library::human(bytes)));
                    }
                    if vids > 0 {
                        label.push_str(&format!(
                            " · {} videos · {}:{:02}:{:02}",
                            vids,
                            ms / 3_600_000,
                            (ms / 60_000) % 60,
                            (ms / 1000) % 60
                        ));
                    }
                    drop(sel);
                    drop(all);
                    drop(sz);
                    ui.label(egui::RichText::new(label).color(RED).strong());
                    if ui.button(if s_shared { "new link" } else { "share" }).clicked() {
                        self.lib_action("share");
                    }
                    if ui.button("download originals").clicked() {
                        self.lib_action("download");
                    }
                    if s_shared && ui.button("manage").clicked() {
                        open_url(&format!("https://photos.google.com/links/{}", &sv[5..]));
                    }
                    if s_trash {
                        if ui.button("restore").clicked() { self.lib_action("restore"); }
                    } else if ui.button(egui::RichText::new("to trash").color(RED)).clicked() {
                        self.lib_action("trash");
                    }
                    if ui.button(if s_archive { "unarchive" } else { "archive" }).clicked() {
                        self.lib_action(if s_archive { "unarchive" } else { "archive" });
                    }
                    if ui.button(if s_favs { "unfavourite" } else { "fav" }).clicked() {
                        self.lib_action(if s_favs { "unfav" } else { "fav" });
                    }
                    if s_locked {
                        if ui.button("unlock").clicked() { self.lib_action("unlock"); }
                    } else if !s_trash && ui.button("lock").clicked() {
                        self.lib_action("lock");
                    }
                    {
                        if !self.confirm_delete {
                            if ui.button(egui::RichText::new("delete forever").color(RED)).clicked() {
                                if self.confirm_del_setting {
                                    self.confirm_delete = true;
                                } else {
                                    self.lib_action("delforever");
                                }
                            }
                        } else {
                            ui.label(egui::RichText::new("PERMANENT - no trash, no undo. This uses an unverified Google action, so double-check the item.").color(RED).strong());
                            if ui.button(egui::RichText::new("YES, delete forever").color(RED)).clicked() {
                                self.confirm_delete = false;
                                self.lib_action("delforever");
                            }
                            if ui.button("cancel").clicked() {
                                self.confirm_delete = false;
                            }
                        }
                    }
                    if sv.starts_with("album:") && ui.button("remove from album").clicked() {
                        self.lib_action("rmalbum");
                    }
                    if ui.button("copy EXIF caption").clicked() { self.lib_action("copydesc"); }
                    if ui.button("date from name").clicked() { self.lib_action("datefromname"); }
                    if ui.button("export CSV").clicked() { self.lib_action("exportmeta"); }
                    ui.label("album:");
                    let alist = self.lib_albums.lock().unwrap().clone();
                    let chosen = self
                        .album_target
                        .as_ref()
                        .and_then(|k| alist.iter().find(|a| &a.key == k).map(|a| a.title.clone()))
                        .unwrap_or_else(|| "new album (type a name)".into());
                    egui::ComboBox::from_id_source("album_target")
                        .selected_text(chosen)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.album_target, None, "new album (type a name)");
                            for a in &alist {
                                ui.selectable_value(
                                    &mut self.album_target,
                                    Some(a.key.clone()),
                                    &a.title,
                                );
                            }
                        });
                    ui.text_edit_singleline(&mut self.album_name);
                    if ui
                        .add_enabled(
                            self.album_target.is_some() || !self.album_name.trim().is_empty(),
                            egui::Button::new("add"),
                        )
                        .clicked()
                    {
                        self.lib_action(if self.album_target.is_some() {
                            "toalbum_existing"
                        } else {
                            "toalbum"
                        });
                    }
                    if ui.button("all").clicked() {
                        self.select_all();
                    }
                    if ui.button("clear").clicked() {
                        self.lib_selected.lock().unwrap().clear();
                    }
                });
                ui.separator();
            }
            if self.current_lib_view().starts_with("link:") {
                ui.horizontal(|ui| {
                    ui.label("items from this shared link:");
                    if ui.button("save to my library").clicked() { self.lib_action("savelink"); }
                });
            }

            // back out of any sub-view, and say where you are
            let cur_here = self.current_lib_view();
            if cur_here == "timeline" {
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Photos per month").size(18.0).strong().color(INK));
                let buckets = self.month_buckets();
                let max = buckets.iter().map(|b| b.count).max().unwrap_or(1).max(1);
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for b in buckets.iter().rev() {
                            let h = (b.count as f32 / max as f32) * 110.0;
                            let (r, resp) = ui.allocate_exact_size(
                                Vec2::new(28.0, 140.0),
                                egui::Sense::click(),
                            );
                            let bar = Rect::from_min_size(
                                Pos2::new(r.left() + 5.0, r.bottom() - h),
                                Vec2::new(18.0, h.max(2.0)),
                            );
                            ui.painter().rect_filled(bar, 2.0, BLUE);
                            ui.painter().text(
                                r.center_bottom() + Vec2::new(0.0, -4.0),
                                Align2::CENTER_BOTTOM,
                                format!("{} '{}", month_name(b.month), b.year % 100),
                                egui::FontId::proportional(8.5),
                                INK_SOFT,
                            );
                            if resp.hovered() {
                                ui.painter().text(
                                    r.center_top() + Vec2::new(0.0, 4.0),
                                    Align2::CENTER_TOP,
                                    b.count.to_string(),
                                    egui::FontId::proportional(9.0),
                                    INK,
                                );
                            }
                            if resp.clicked() {
                                let (y, m) = (b.year, b.month);
                                let items: Vec<library::MediaItem> = self
                                    .all_items
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .filter(|i| {
                                        i.ts > 0
                                            && {
                                                let (iy, im, _) =
                                                    library::civil_from_days(i.ts.div_euclid(86400));
                                                iy == y && im == m
                                            }
                                    })
                                    .cloned()
                                    .collect();
                                if !items.is_empty() {
                                    self.show_subset(&format!("month:{}-{}", y, m), items, true);
                                }
                            }
                        }
                    });
                });
                ui.separator();
                return;
            }
            let album_key = cur_here.strip_prefix("album:").map(|k| k.to_string());
            let busy_here = self.lib_busy.load(Ordering::Relaxed) && !self.warm_active;
            if cur_here != "recent" || busy_here {
                ui.horizontal(|ui| {
                    if cur_here != "recent" {
                        if ui.button("< back").clicked() {
                            let p = self.view_prev.lock().unwrap().clone();
                            if let Some(snap) = self.view_snapshots.lock().unwrap().remove(&p) {
                                *self.view_prev.lock().unwrap() = cur_here.clone();
                                *self.lib_view.lock().unwrap() = p;
                                *self.lib_items.lock().unwrap() = snap;
                                self.items_gen.fetch_add(1, Ordering::Relaxed);
                                self.lib_next.lock().unwrap().take();
                            } else {
                                self.fetch_lib(p);
                            }
                        }
                    }
                    let title = if cur_here == "timeline" {
                        "timeline".to_string()
                    } else if let Some(mm) = cur_here.strip_prefix("month:") {
                        format!("photos from {}", mm)
                    } else if let Some(y) = cur_here.strip_prefix("year:") {
                        format!("photos from {}", y)
                    } else if cur_here == "videos" {
                        "videos only".to_string()
                    } else if cur_here == "photos" {
                        "photos only".to_string()
                    } else if cur_here == "bursts" {
                        "bursts / stacks".to_string()
                    } else if cur_here == "large" {
                        format!("large files (≥ {} MB)", LARGE_FILE_BYTES / (1024 * 1024))
                    } else if let Some(f) = cur_here.strip_prefix("filter:") {
                        format!("{} (≥ {} MB)", f.replace("+large", "").replace('+', " + "),
                            LARGE_FILE_BYTES / (1024 * 1024))
                    } else if let Some(ext) = cur_here.strip_prefix("type:") {
                        format!("{} files", ext)
                    } else if cur_here == "dupes" {
                        "duplicate candidates".to_string()
                    } else if cur_here.starts_with("filename:") {
                        format!("files named “{}”", &cur_here[9..])
                    } else if let Some(k) = &album_key {
                        let name = self
                            .lib_albums
                            .lock()
                            .unwrap()
                            .iter()
                            .find(|a| &a.key == k)
                            .map(|a| a.title.clone())
                            .unwrap_or_else(|| "album".into());
                        format!("{} · {}", name, k)
                    } else if cur_here.starts_with("album:") {
                        "album".to_string()
                    } else if cur_here.starts_with("search:") {
                        format!("search \"{}\"", &cur_here[7..])
                    } else if cur_here == "shared" {
                        "shared albums".to_string()
                    } else if cur_here == "links" {
                        "shared links".to_string()
                    } else if cur_here == "things" {
                        "things & places".to_string()
                    } else if cur_here.starts_with("link:") {
                        "shared link".to_string()
                    } else if cur_here == "hogs" {
                        "biggest items".to_string()
                    } else {
                        cur_here.clone()
                    };
                    ui.label(egui::RichText::new(title).size(14.0).strong());
                    if busy_here {
                        ui.label(egui::RichText::new("loading…").size(12.0).color(INK_SOFT));
                    }
                });
                ui.separator();
            }
            if let Some(k) = &album_key {
                let tx = self.tx.clone();
                let web = format!("https://photos.google.com/album/{}", k);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Open on web").clicked() {
                        open_url(&web);
                    }
                    if ui.button("Copy link").clicked() {
                        let _ = copy_text_to_clipboard(&web);
                        tx.send(Msg::Toast {
                            msg: "album link copied".into(),
                            is_error: false,
                        })
                        .ok();
                    }
                    if ui.button("Share album").clicked() {
                        self.album_selected.clear();
                        self.album_selected.insert(k.clone());
                        self.album_action("share");
                    }
                    if ui.button("Manage").clicked() {
                        open_url(&web);
                        tx.send(Msg::Toast {
                            msg: "opened album sharing settings on the web".into(),
                            is_error: false,
                        })
                        .ok();
                    }
                });
            }

            if cur_here == "recent" || cur_here.starts_with("search:")
                || cur_here == "favorites" || cur_here == "archive"
                || cur_here == "trash" || cur_here == "locked"
                || matches!(cur_here.as_str(), "videos" | "photos" | "large" | "dupes")
                || cur_here.starts_with("filter:")
                || cur_here.starts_with("year:")
                || cur_here.starts_with("month:")
                || cur_here.starts_with("type:")
                || cur_here.starts_with("filename:")
            {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("sort:").size(11.5).color(INK_SOFT));
                    let names = ["newest", "oldest", "name", "largest"];
                    egui::ComboBox::from_id_source("grid_sort")
                        .selected_text(names[self.grid_sort as usize])
                        .show_ui(ui, |ui| {
                            for (i, n) in names.iter().enumerate() {
                                if ui.selectable_value(&mut self.grid_sort, i as u8, *n).clicked() {
                                    set_setting("grid_sort", &i.to_string());
                                }
                            }
                        });
                });
            }
            let mut grid_sa = egui::ScrollArea::vertical()
                .id_salt("photos_grid")
                .auto_shrink([false, false])
                .scroll_bar_visibility(
                    egui::containers::scroll_area::ScrollBarVisibility::AlwaysHidden,
                )
                .animated(false);
            if let Some(off) = self.grid_want.take() {
                grid_sa = grid_sa.vertical_scroll_offset(off);
            }
            let grid_out = grid_sa.show(ui, |ui| {
                    // albums / shared-links chips
                    {
                        let albums = self.lib_albums.lock().unwrap().clone();
                        let cur = self.current_lib_view();
                        let listing = cur == "albums" || cur == "shared";
                        if listing {
                            if albums.is_empty() && !self.lib_busy.load(Ordering::Relaxed) {
                                let msg = if cur == "shared" {
                                    "no shared albums yet"
                                } else {
                                    "no albums yet"
                                };
                                ui.label(egui::RichText::new(msg).color(INK_SOFT));
                            }
                            ui.horizontal_wrapped(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.album_name)
                                        .hint_text("new album name")
                                        .desired_width(160.0),
                                );
                                if ui
                                    .add_enabled(
                                        !self.album_name.trim().is_empty(),
                                        egui::Button::new("new album"),
                                    )
                                    .clicked()
                                {
                                    self.create_album_named();
                                }
                                if ui.button("all albums").clicked() {
                                    self.album_selected = albums.iter().map(|a| a.key.clone()).collect();
                                }
                                if !self.album_selected.is_empty() {
                                    ui.label(format!("{} selected", self.album_selected.len()));
                                    if ui.button("open").clicked() {
                                        if let Some(k) = self.album_selected.iter().next() {
                                            let v = format!("album:{}", k);
                                            self.fetch_lib(v);
                                        }
                                    }
                                    if ui.button("share").clicked() {
                                        self.album_action("share");
                                    }
                                    if ui.button("copy web link").clicked() {
                                        self.album_action("web");
                                    }
                                    if !self.confirm_album_del {
                                        if ui.button(egui::RichText::new("delete album").color(RED)).clicked() {
                                            self.confirm_album_del = true;
                                        }
                                    } else {
                                        if ui.button(egui::RichText::new("YES, delete albums").color(RED)).clicked() {
                                            self.confirm_album_del = false;
                                            self.album_action("delete");
                                        }
                                        if ui.button("cancel").clicked() {
                                            self.confirm_album_del = false;
                                        }
                                    }
                                    if ui.button("clear").clicked() {
                                        self.album_selected.clear();
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("sort:");
                                egui::ComboBox::from_id_source("album_sort")
                                    .selected_text(["name A–Z", "name Z–A", "most items"][self.album_sort as usize])
                                    .show_ui(ui, |ui| {
                                        if ui.selectable_value(&mut self.album_sort, 0, "name").clicked() {
                                            set_setting("album_sort", "0");
                                        }
                                        if ui.selectable_value(&mut self.album_sort, 1, "name Z–A").clicked() {
                                            set_setting("album_sort", "1");
                                        }
                                        if ui.selectable_value(&mut self.album_sort, 2, "most items").clicked() {
                                            set_setting("album_sort", "2");
                                        }
                                    });
                            });
                            let mut albums = albums;
                            match self.album_sort {
                                1 => albums.sort_by(|a, b| b.title.cmp(&a.title)),
                                2 => albums.sort_by(|a, b| b.count.cmp(&a.count)),
                                _ => albums.sort_by(|a, b| a.title.cmp(&b.title)),
                            }
                            ui.horizontal_wrapped(|ui| {
                                for a in &albums {
                                    self.draw_album_cell(ui, a, self.album_selected.contains(&a.key));
                                    ui.add_space(8.0);
                                }
                            });
                        } else if cur == "things" {
                            let things = self.lib_things.lock().unwrap().clone();
                            if things.is_empty() {
                                ui.label(egui::RichText::new("no things found — refresh to rescan").color(INK_SOFT));
                            }
                            ui.horizontal_wrapped(|ui| {
                                for t in &things {
                                    ui.group(|ui| {
                                        ui.vertical(|ui| {
                                            ui.label(egui::RichText::new(&t.name).size(12.5).strong());
                                            ui.label(egui::RichText::new(
                                                if t.is_place { "place" } else { "thing" }
                                            ).size(10.0).color(INK_SOFT));
                                            if ui.small_button("show photos").clicked() {
                                                self.run_search(&t.name);
                                            }
                                        });
                                    });
                                    ui.add_space(8.0);
                                }
                            });
                        } else if cur == "links" {
                            let links = self.lib_links.lock().unwrap().clone();
                            if links.is_empty() {
                                ui.label(egui::RichText::new("no shared links yet").color(INK_SOFT));
                            }
                            for l in links {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!("{} · {} items", truncate(&l.key, 28), l.count))
                                            .size(13.0)
                                            .color(INK),
                                    );
                                    if ui.button("open").clicked() {
                                        self.fetch_lib(format!("link:{}", l.key));
                                    }
                                    if ui.button("open on web").clicked() {
                                        open_url(&format!("https://photos.google.com/links/{}", l.key));
                                    }
                                    if ui.button("copy link").clicked() {
                                        let url = format!("https://photos.google.com/links/{}", l.key);
                                        let _ = copy_text_to_clipboard(&url);
                                        self.tx.send(Msg::Toast {
                                            msg: "link copied".into(),
                                            is_error: false,
                                        }).ok();
                                    }
                                });
                            }
                            ui.add_space(10.0);
                        }
                    }

                    // the photo grid, date-grouped like the real app
                    let cur_view = self.current_lib_view();
                    // rebuild the snapshot only when the list actually changed;
                    // the Arc lets us hand &MediaItem to the cell painter without
                    // borrowing `self` or re-cloning thousands of rows per frame
                    let gen = self.items_gen.load(Ordering::Relaxed);
                    if gen != self.grid_cache_gen {
                        self.grid_cache = Arc::new(self.lib_items.lock().unwrap().clone());
                        self.grid_cache_gen = gen;
                    }
                    let snap = self.grid_cache.clone();
                    let sel_set = self.lib_selected.lock().unwrap().clone();
                    let busy = self.lib_busy.load(Ordering::Relaxed);
                    self.tex_budget = 64;
                    // storage-hogs view: filter by min size, biggest first
                    let hog_view = cur_view == "hogs";
                    let burst_view = cur_view == "bursts";
                    let mut hog_rows: Vec<library::MediaItem> = vec![];
                    let sizes_map = if hog_view {
                        self.sizes.lock().unwrap().clone()
                    } else {
                        Default::default()
                    };
                    if hog_view {
                        let min_bytes: u64 = (self.hog_min_mb.trim().parse::<f64>().unwrap_or(0.0) * 1_048_576.0) as u64;
                        hog_rows = snap
                            .iter()
                            .filter(|i| *sizes_map.get(&i.media_key).unwrap_or(&0) >= min_bytes)
                            .cloned()
                            .collect();
                        hog_rows.sort_by(|a, b| {
                            sizes_map
                                .get(&b.media_key)
                                .copied()
                                .unwrap_or(0)
                                .cmp(&sizes_map.get(&a.media_key).copied().unwrap_or(0))
                        });
                        let total: u64 = hog_rows.iter().map(|i| *sizes_map.get(&i.media_key).unwrap_or(&0)).sum();
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!(
                                "biggest first — {} of {} items · {}",
                                hog_rows.len(),
                                self.hog_stats.as_ref().map(|s| s.total_n).unwrap_or(hog_rows.len()),
                                library::human(total)
                            )).strong());
                            ui.label("bigger than (MB):");
                            ui.add(egui::TextEdit::singleline(&mut self.hog_min_mb).desired_width(70.0));
                            let mut years: Vec<i64> = self
                                .hog_stats
                                .as_ref()
                                .map(|s| s.years.iter().map(|(y, _, _)| *y).collect())
                                .unwrap_or_default();
                            years.sort_by(|a, b| b.cmp(a));
                            egui::ComboBox::from_id_source("hog_year")
                                .selected_text(
                                    self.hog_year
                                        .map(|y| y.to_string())
                                        .unwrap_or_else(|| "all years".into()),
                                )
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.hog_year, None, "all years");
                                    for y in years {
                                        ui.selectable_value(&mut self.hog_year, Some(y), y.to_string());
                                    }
                                });
                        });
                        if let Some(y) = self.hog_year {
                            hog_rows.retain(|i| {
                                i.ts > 0 && library::civil_from_days(i.ts.div_euclid(86400)).0 == y
                            });
                        }
                        if let Some(st) = &self.hog_stats {
                            self.storage_report(ui, st);
                        } else {
                            ui.label(egui::RichText::new("measuring every list…").color(INK_SOFT));
                        }
                        ui.separator();
                    }
                    // apply the chosen sort order to a local copy; the Arc snapshot
                    // itself is shared, so we never mutate the cached data
                    let mut sorted: Vec<library::MediaItem> = vec![];
                    let owned_items: Vec<library::MediaItem> = if hog_view {
                        hog_rows.clone()
                    } else if burst_view || self.grid_sort == 0 {
                        // bursts must stay newest-first so each group's tiles
                        // are contiguous under one header, whatever grid_sort is
                        (*snap).clone()
                    } else {
                        let key = (gen, self.grid_sort);
                            if self.sorted_cache_key != key || self.sorted_cache.is_empty() {
                                self.sorted_cache = (*snap).clone();
                                match self.grid_sort {
                                    1 => self.sorted_cache.sort_by(|a, b| a.ts.cmp(&b.ts)),
                                    2 => {
                                        let b = self.bulk.lock().unwrap();
                                        self.sorted_cache.sort_by(|a, z| {
                                            let na = b.get(&a.media_key).map(|x| x.0.as_str()).unwrap_or("");
                                            let nz = b.get(&z.media_key).map(|x| x.0.as_str()).unwrap_or("");
                                            na.cmp(nz)
                                        });
                                    }
                                    3 => {
                                        let s = self.sizes.lock().unwrap();
                                        self.sorted_cache.sort_by(|a, z| {
                                            s.get(&z.media_key).copied().unwrap_or(0)
                                                .cmp(&s.get(&a.media_key).copied().unwrap_or(0))
                                        });
                                    }
                                    _ => {}
                                }
                                self.sorted_cache_key = key;
                            }
                        self.sorted_cache.clone()
                    };
                    let items: &[library::MediaItem] = &owned_items;
                    // date headers only make sense for chronological order
                    let group_by_date = self.grid_sort == 0 || hog_view;
                    let listing_view = cur_view == "albums" || cur_view == "shared"
                        || cur_view == "links" || cur_view == "things";
                    if items.is_empty() && !busy && !listing_view {
                        ui.add_space(20.0);
                        let listing = listing_view;
                        let msg = if hog_view {
                            if self.hog_stats.is_none() {
                                "Scanning your library…".to_string()
                            } else {
                                "no items above that size".to_string()
                            }
                        } else if listing {
                            "none yet".to_string()
                        } else {
                            match cur_view.as_str() {
                                "favorites" => "no favorites yet — star a photo to see it here".to_string(),
                                "archive" => "archive is empty — archived photos are hidden, not deleted".to_string(),
                                "trash" => "trash is empty".to_string(),
                                "locked" => "locked folder is empty".to_string(),
                                "dupes" => "no duplicate candidates found — nothing to review".to_string(),
                                "bursts" => "no bursts found — 3+ shots within 3 s with the same size and type".to_string(),
                                v if v.starts_with("filename:") => format!("no files named \"{}\"", &v[9..]),
                                v if v.starts_with("type:") => format!("no {} files", &v[5..]),
                                v if v.starts_with("search:") => format!("no results for \"{}\"", &v[7..]),
                                v if self.all_items.lock().unwrap().is_empty()
                                    && (matches!(v, "videos" | "photos" | "large")
                                        || v.starts_with("filter:")
                                        || v.starts_with("year:")
                                        || v.starts_with("month:")) =>
                                {
                                    "this filter fills in after the one-time library scan — refresh to start it".to_string()
                                }
                                v if v.starts_with("filter:") =>
                                    "no items match these filters — turn one off to widen the view".to_string(),
                                _ => "loading your photos… (first refresh may take a moment)".to_string(),
                            }
                        };
                        ui.label(egui::RichText::new(msg).color(INK_SOFT));
                    }
                    // first page still in flight: hold the grid's shape
                    if items.is_empty() && busy && !listing_view && !hog_view {
                        ui.add_space(10.0);
                        let ts = self.thumb_size;
                        for _ in 0..3 {
                            ui.horizontal_wrapped(|ui| {
                                for _ in 0..14 {
                                    Self::draw_skeleton_cell(ui, ts);
                                }
                            });
                        }
                    }
                    // the photo grid: date groups, tiles wrapping to the
                    // window width exactly — nothing hides past the edge
                    {
                        let mut groups: Vec<(std::sync::Arc<str>, Vec<&library::MediaItem>)> = vec![];
                        if burst_view {
                            // label is the unique group index; the human-readable
                            // header is built from the group's first item below
                            for it in items.iter() {
                                let label: std::sync::Arc<str> = self
                                    .burst_info
                                    .get(&it.media_key)
                                    .map(|g| g.0.to_string().into())
                                    .unwrap_or_else(|| "".into());
                                match groups.last_mut() {
                                    Some(g) if g.0 == label => g.1.push(it),
                                    _ => groups.push((label, vec![it])),
                                }
                            }
                        } else if group_by_date {
                            for it in items.iter() {
                                let label = self.cached_date_label(it.ts);
                                match groups.last_mut() {
                                    Some(g) if g.0 == label => g.1.push(it),
                                    _ => groups.push((label, vec![it])),
                                }
                            }
                        } else {
                            groups.push(("".into(), items.iter().collect()));
                        }
                        for (label, gitems) in groups {
                            if !label.is_empty() {
                                ui.add_space(8.0);
                                let header = if burst_view {
                                    let first = gitems[0];
                                    let n = self.burst_info.get(&first.media_key).map(|g| g.1).unwrap_or(0);
                                    let date = self.cached_date_label(first.ts);
                                    format!("{} shots · {} · {}×{}", n, date, first.w, first.h)
                                } else {
                                    label.to_string()
                                };
                                ui.label(egui::RichText::new(header).size(15.0).strong().color(INK));
                            }
                            ui.horizontal_wrapped(|ui| {
                                for it in gitems {
                                    if hog_view {
                                        ui.vertical(|ui| {
                                            self.draw_thumb_cell(ui, it, sel_set.contains(&it.media_key), items);
                                            let b = *sizes_map.get(&it.media_key).unwrap_or(&0) as f64;
                                            let mut txt = library::human(b as u64);
                                            if it.is_video && it.duration_ms > 0 {
                                                let secs = it.duration_ms as f64 / 1000.0;
                                                txt.push_str(&format!(
                                                    " · {}:{:02} · {:.0} Mbps",
                                                    (secs as u64) / 60,
                                                    (secs as u64) % 60,
                                                    b * 8.0 / secs / 1_000_000.0
                                                ));
                                            }
                                            ui.label(egui::RichText::new(txt).size(10.0).color(INK_SOFT));
                                            if self
                                                .charged
                                                .lock()
                                                .unwrap()
                                                .contains(&it.media_key)
                                            {
                                                ui.label(
                                                    egui::RichText::new("uses storage")
                                                        .size(10.0)
                                                        .color(Color32::from_rgb(180, 40, 40)),
                                                );
                                            }
                                        });
                                    } else {
                                        self.draw_thumb_cell(ui, it, sel_set.contains(&it.media_key), items);
                                    }
                                }
                            });
                        }
                    }
                    if !items.is_empty() && self.lib_next.lock().unwrap().is_some() {
                        ui.add_space(8.0);
                        let more = ui.add_enabled(
                            !busy,
                            egui::Button::new(if busy { "loading…" } else { "load more" }),
                        );
                        if more.clicked() {
                            self.fetch_more();
                        } else if !busy
                            && self.last_more.elapsed() > Duration::from_secs(2)
                            && ui.is_rect_visible(more.rect)
                        {
                            // the end of the grid is on screen: page in silently
                            self.last_more = Instant::now();
                            self.fetch_more();
                        }
                    }
                    ui.add_space(20.0);
            });
            self.grid_view_h = grid_out.inner_rect.height();
            self.grid_content_h = grid_out.content_size.y;
            if std::env::var("GPHOTOS_SCROLLDBG").is_ok() {
                eprintln!("[SD] grid off={:.0} view={:.0} content={:.0} frac={:.3}", grid_out.state.offset.y, self.grid_view_h, self.grid_content_h, self.grid_frac);
            }
            if !self.grid_drag {
                let max = (self.grid_content_h - self.grid_view_h).max(0.0);
                self.grid_frac = if max > 0.0 {
                    (grid_out.state.offset.y / max).clamp(0.0, 1.0)
                } else {
                    0.0
                };
            }
        });
    }

    // placeholder block while a thumbnail is still coming down: the grid keeps its
// shape instead of jumping from empty to full
fn draw_skeleton_cell(ui: &mut egui::Ui, size: f32) {
    let (cell, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let t = ui.ctx().input(|i| i.time) as f32;
    let pulse = 0.5 + 0.5 * (t * 1.6).sin();
    let p = ui.painter_at(cell);
    p.rect_filled(cell, 3.0, PAPER_DARK);
    p.rect_filled(
        cell.shrink(1.0),
        3.0,
        Color32::from_rgb(0xdc, 0xd2, 0xbf).gamma_multiply(0.55 + 0.25 * pulse),
    );
    ui.add_space(6.0);
}

// one photo cell: click to select, sketchy frame
    fn draw_thumb_cell(
        &mut self,
        ui: &mut egui::Ui,
        it: &library::MediaItem,
        selected: bool,
        shown: &[library::MediaItem],
    ) {
        let (cell, resp) = ui.allocate_exact_size(Vec2::splat(self.thumb_size), egui::Sense::click());
        // a jump target may be offscreen, so ask for the scroll BEFORE the
        // offscreen early return (scroll_to_rect is how you reach it)
        if self.scroll_anchor.as_deref() == Some(it.media_key.as_str()) {
            if std::env::var("GPHOTOS_SCROLLDBG").is_ok() {
                eprintln!("[SD] anchor key={} visible={}", it.media_key, ui.is_rect_visible(cell));
            }
            ui.scroll_to_rect(cell, Some(egui::Align::Center));
            self.prewarm_thumbs_around(&it.media_key);
            self.scroll_anchor = None;
        }
        if self.jump_hook.as_deref() == Some(it.media_key.as_str()) {
            ui.scroll_to_rect(cell, Some(egui::Align::TOP));
        }
        // a library of thousands is laid out every frame; only the cells that
        // are actually on screen pay for texture lookups and text
        if !ui.is_rect_visible(cell) {
            return;
        }
        let painter = ui.painter_at(cell);
        painter.rect_filled(cell, 3.0, if selected { YELLOW } else { PAPER_DARK });
        if selected {
            painter.rect_stroke(cell, 3.0, Stroke::new(3.0, RED));
        }
        let mut tex_id = None;
        let thumb_img = {
            let guard = self.thumbs.lock().unwrap();
            guard.get(&it.media_key).cloned()
        };
        if let Some(img) = thumb_img {
            if let Some(tex) = self.tex.get(&it.media_key) {
                tex_id = Some(tex.id());
            } else if self.tex_budget > 0 {
                self.tex_budget -= 1;
                let tex = ui.ctx().load_texture(&it.media_key, img.clone(), egui::TextureOptions::LINEAR);
                tex_id = Some(tex.id());
                self.tex.insert(it.media_key.clone(), tex);
            }
        }
        if tex_id.is_none() {
            let t = ui.ctx().input(|i| i.time) as f32;
            let pulse = 0.5 + 0.5 * (t * 1.6).sin();
            painter.rect_filled(
                cell.shrink(1.0),
                3.0,
                Color32::from_rgb(0xdc, 0xd2, 0xbf).gamma_multiply(0.55 + 0.25 * pulse),
            );
        }
        if let Some(id) = tex_id {
            let tex = self.tex.get(&it.media_key).unwrap();
            let tw = (tex.size()[0] as f32).max(1.0);
            let th = (tex.size()[1] as f32).max(1.0);
            // cover, not contain: a portrait photo fills the square tile
            let scale = (cell.width() / tw).max(cell.height() / th);
            let size = Vec2::new(tw * scale, th * scale);
            let pos = cell.center() - size * 0.5;
            painter.image(
                id,
                Rect::from_min_size(pos, size),
                Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        // one widget per cell: click in the corner circle toggles selection,
        // a click anywhere else opens the photo full screen
        let corner = Rect::from_min_size(cell.left_top() + Vec2::new(6.0, 6.0), Vec2::splat(20.0));
        painter.circle_filled(corner.center(), 9.0, if selected { YELLOW } else { Color32::from_black_alpha(90) });
        if selected {
            // a real check mark: down from the left, long leg up to the right
            let c = corner.center();
            painter.line_segment(
                [c + Vec2::new(-5.0, 0.0), c + Vec2::new(-1.5, 4.0)],
                Stroke::new(2.5, INK),
            );
            painter.line_segment(
                [c + Vec2::new(-1.5, 4.0), c + Vec2::new(4.5, -4.0)],
                Stroke::new(2.5, INK),
            );
        }
        // test hook: GPHOTOS_HOVER=<media key> previews one tile without a pointer
        let forced = std::env::var("GPHOTOS_HOVER")
            .map(|k| k == it.media_key)
            .unwrap_or(false);
        if it.is_video && self.viewer.is_none() && (resp.hovered() || forced) {
            self.prewarm_tick(it);
            self.hover_want = Some((it.media_key.clone(), it.thumb.clone()));
        }
        // the preview plays inside the tile, over the still, under the badges
        if let Some(hv) = self.hover_vid.as_ref().filter(|h| h.key == it.media_key) {
            if let Some(t) = &hv.tex {
                let tw = (t.size()[0] as f32).max(1.0);
                let th = (t.size()[1] as f32).max(1.0);
                let scale = (cell.width() / tw).max(cell.height() / th);
                let size = Vec2::new(tw * scale, th * scale);
                painter.image(
                    t.id(),
                    Rect::from_min_size(cell.center() - size * 0.5, size),
                    Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        }
        // videos get the play button the phone app shows, dead center
        let play_zone = Rect::from_center_size(cell.center(), Vec2::splat(36.0));
        if it.is_video {
            let hovered = resp
                .hover_pos()
                .map(|p| play_zone.contains(p))
                .unwrap_or(false);
            painter.circle_filled(
                cell.center(),
                11.0,
                if hovered {
                    Color32::from_white_alpha(245)
                } else {
                    Color32::from_white_alpha(185)
                },
            );
            let c = cell.center();
            painter.add(egui::Shape::convex_polygon(
                vec![
                    c + Vec2::new(-3.0, -5.0),
                    c + Vec2::new(5.0, 0.0),
                    c + Vec2::new(-3.0, 5.0),
                ],
                INK,
                Stroke::NONE,
            ));
            if hovered {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if it.duration_ms > 0 {
                let s = it.duration_ms / 1000;
                let label = if s >= 3600 {
                    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
                } else {
                    format!("{}:{:02}", s / 60, s % 60)
                };
                let galley = painter.layout_no_wrap(
                    label,
                    egui::FontId::proportional(10.0),
                    Color32::WHITE,
                );
                let size = galley.size();
                let pill = Rect::from_min_size(
                    Pos2::new(cell.right() - size.x - 13.0, cell.bottom() - 19.0),
                    Vec2::new(size.x + 7.0, 14.0),
                );
                painter.rect_filled(pill, 4.0, Color32::from_black_alpha(160));
                painter.galley(pill.center() - size * 0.5, galley, Color32::WHITE);
            }
        }
        // a small size chip for large files, so you can spot the space hogs
        // in the normal grid too (hogs view already prints sizes below tiles)
        if self.current_lib_view() != "hogs" && self.thumb_size >= 120.0 {
            let sz = self.sizes.lock().unwrap().get(&it.media_key).copied().unwrap_or(0);
            if sz >= 50_000_000 {
                let galley = painter.layout_no_wrap(
                    library::human(sz),
                    egui::FontId::proportional(10.0),
                    Color32::WHITE,
                );
                let chip = Rect::from_min_size(
                    Pos2::new(cell.left() + 4.0, cell.bottom() - 20.0),
                    Vec2::new(galley.size().x + 8.0, 17.0),
                );
                painter.rect_filled(chip, 4.0, Color32::from_black_alpha(170));
                painter.galley(chip.left_top() + Vec2::new(4.0, 2.0), galley, Color32::WHITE);
            }
        }
        // bursts view: a small ×N badge so each tile shows its group size
        if self.current_lib_view() == "bursts" {
            if let Some(n) = self.burst_info.get(&it.media_key).map(|g| g.1) {
                let galley = painter.layout_no_wrap(
                    format!("×{}", n),
                    egui::FontId::proportional(11.0),
                    Color32::WHITE,
                );
                let size = galley.size();
                let badge = Rect::from_min_size(
                    Pos2::new(cell.right() - size.x - 12.0, cell.top() + 4.0),
                    Vec2::new(size.x + 8.0, 17.0),
                );
                painter.rect_filled(badge, 4.0, Color32::from_black_alpha(170));
                painter.galley(badge.left_top() + Vec2::new(4.0, 2.0), galley, Color32::WHITE);
            }
        }
        // single click opens, as before. ctrl/cmd-click or the corner circle
        // selects; shift-click takes the whole range from the last one picked.
        if resp.clicked() {
            let mods = ui.ctx().input(|i| i.modifiers);
            let pos = resp.interact_pointer_pos();
            let corner_click = pos.map(|p| corner.contains(p)).unwrap_or(false);
            if mods.ctrl || mods.command || mods.shift || corner_click {
                let mut s = self.lib_selected.lock().unwrap();
                if mods.shift {
                    if let Some(a) = &self.sel_anchor {
                        if let (Some(i), Some(j)) = (
                            shown.iter().position(|x| x.media_key == *a),
                            shown.iter().position(|x| x.media_key == it.media_key),
                        ) {
                            let (lo, hi) = if i <= j { (i, j) } else { (j, i) };
                            for x in &shown[lo..=hi] {
                                s.insert(x.media_key.clone());
                            }
                        }
                    }
                } else if !s.remove(&it.media_key) {
                    s.insert(it.media_key.clone());
                    self.sel_anchor = Some(it.media_key.clone());
                }
            } else {
                self.open_viewer(&it.media_key);
            }
        }
    
        // right-click gives the same link actions without leaving the grid
        {
            let tx = self.tx.clone();
            let key = it.media_key.clone();
            let web = format!("https://photos.google.com/photo/{}", key);
            resp.context_menu(|ui| {
                if std::env::var("GPHOTOS_CTXDBG").is_ok() {
                    eprintln!("[CTX] photo menu opened");
                }
                if ui.button("Open on web").clicked() {
                    open_url(&web);
                    ui.close_menu();
                }
                if ui.button("Copy link").clicked() {
                    let _ = copy_text_to_clipboard(&web);
                    tx.send(Msg::Toast { msg: "link copied".into(), is_error: false }).ok();
                    ui.close_menu();
                }
                if ui.button("Share").clicked() {
                    self.act_on_one(&key, "share");
                    ui.close_menu();
                }
                if ui.button("Details").clicked() {
                    self.open_viewer(&key);
                    ui.close_menu();
                }
                if ui.button("Download").clicked() {
                    self.act_on_one(&key, "download");
                    ui.close_menu();
                }
                if ui.button(if it.is_favorite { "Unfavourite" } else { "Favourite" }).clicked() {
                    self.act_on_one(&key, if it.is_favorite { "unfav" } else { "fav" });
                    ui.close_menu();
                }
                let cur = self.current_lib_view();
                if cur == "locked" {
                    if ui.button("Unlock").clicked() {
                        self.act_on_one(&key, "unlock");
                        ui.close_menu();
                    }
                } else if cur != "trash" {
                    if ui.button(if cur == "archive" { "Unarchive" } else { "Archive" }).clicked() {
                        self.act_on_one(&key, if cur == "archive" { "unarchive" } else { "archive" });
                        ui.close_menu();
                    }
                }
                if ui.button(if cur == "trash" { "Restore" } else { "Move to trash" }).clicked() {
                    self.act_on_one(&key, if cur == "trash" { "restore" } else { "trash" });
                    ui.close_menu();
                }
            });
        }
}

    fn create_album_named(&mut self) {
        let name = self.album_name.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.album_name.clear();
        let Some(s) = self.lib_session.lock().unwrap().clone() else { return };
        let tx = self.tx.clone();
        thread::spawn(move || {
            let made = library::create_album(&s, &name);
            tx.send(Msg::Toast {
                msg: if made.is_some() {
                    format!("album “{}” created", name)
                } else {
                    "could not create the album".into()
                },
                is_error: made.is_none(),
            })
            .ok();
            if made.is_some() {
                tx.send(Msg::RefreshLib).ok();
            }
        });
    }

    // an action on the selected albums, never on their photos
    fn album_action(&self, action: &str) {
        let keys: Vec<String> = self.album_selected.iter().cloned().collect();
        if keys.is_empty() {
            return;
        }
        let Some(s) = self.lib_session.lock().unwrap().clone() else { return };
        let tx = self.tx.clone();
        let copy_links = self.copy_links;
        let act = action.to_string();
        thread::spawn(move || {
            match act.as_str() {
                "delete" => {
                    let mut n = 0;
                    for k in &keys {
                        if library::delete_album(&s, k) {
                            n += 1;
                        }
                    }
                    tx.send(Msg::Toast {
                        msg: format!("{} album(s) deleted — the photos are untouched", n),
                        is_error: n == 0,
                    })
                    .ok();
                    tx.send(Msg::RefreshLib).ok();
                }
                "web" => {
                    let url = format!("https://photos.google.com/album/{}", keys[0]);
                    open_url(&url);
                    tx.send(Msg::Toast {
                        msg: "opened on the web".into(),
                        is_error: false,
                    })
                    .ok();
                }
                "share" => {
                    let mut made = 0;
                    for k in &keys {
                        let (inside, _) = library::album_items(&s, k, None);
                        let media: Vec<String> = inside.iter().map(|i| i.media_key.clone()).collect();
                        if media.is_empty() {
                            continue;
                        }
                        if let Some(url) = library::create_share_link(&s, &media) {
                            copy_text_to_clipboard(&url);
                            tx.send(Msg::Toast {
                                msg: format!("album link copied: {}", url),
                                is_error: false,
                            })
                            .ok();
                            made += 1;
                            break;
                        }
                    }
                    if made == 0 {
                        tx.send(Msg::Toast {
                            msg: "no link — the album may be empty".into(),
                            is_error: true,
                        })
                        .ok();
                    }
                }
                _ => {}
            }
        });
    }

    // album / shared-album tile: cover, name, item count; click opens it

    fn draw_album_cell(&mut self, ui: &mut egui::Ui, a: &library::Album, selected: bool) {
        let tx = self.tx.clone();
        let akey = a.key.clone();
        let (cell, resp) = ui.allocate_exact_size(Vec2::new(150.0, 150.0), egui::Sense::click());
        let painter = ui.painter_at(cell);
        let key = format!("album:{}", a.key);
        painter.rect_filled(cell, 4.0, if selected { YELLOW } else { PAPER_DARK });
        painter.rect_stroke(
            cell,
            4.0,
            Stroke::new(if selected { 3.0 } else { 2.0 }, if selected { RED } else { INK }),
        );
        let thumb_img = {
            let guard = self.thumbs.lock().unwrap();
            guard.get(&key).cloned()
        };
        if let Some(img) = thumb_img {
            let tex = match self.tex.get(&key) {
                Some(t) => Some(t.clone()),
                None => {
                    let t = ui.ctx().load_texture(&key, img.clone(), egui::TextureOptions::LINEAR);
                    self.tex.insert(key.clone(), t.clone());
                    Some(t)
                }
            };
            if let Some(tex) = tex {
                let tw = (tex.size()[0] as f32).max(1.0);
                let th = (tex.size()[1] as f32).max(1.0);
                let scale = ((cell.width() - 8.0) / tw).min((cell.height() - 8.0) / th);
                let size = Vec2::new(tw * scale, th * scale);
                let pos = cell.center() - size * 0.5;
                painter.image(
                    tex.id(),
                    Rect::from_min_size(pos, size),
                    Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        } else {
            painter.text(
                cell.center(),
                egui::Align2::CENTER_CENTER,
                "album",
                egui::FontId::proportional(16.0),
                INK_SOFT,
            );
        }
        // shared albums carry a paperclip: centreline traced from SVG Repo
        // "Clip 2" (id 521107, CC0/public domain), scaled into a 16px box
        if a.is_shared {
            let q = cell.right_top() + Vec2::new(-13.0, 13.0);
            painter.circle_filled(q, 10.0, Color32::from_black_alpha(150));
            const CLIP: &[(f32, f32)] = &[
                (6.25, 4.77), (6.25, 10.31), (7.0, 11.60), (8.00, 12.15),
                (9.00, 11.60), (9.75, 10.31), (9.75, 4.77), (9.10, 2.90),
                (7.13, 2.14), (5.27, 2.90), (4.50, 4.77), (4.50, 10.31),
                (5.53, 12.78), (8.00, 13.81), (10.47, 12.78), (11.50, 10.31),
                (11.50, 4.77),
            ];
            let pts: Vec<Pos2> = CLIP
                .iter()
                .map(|&(x, y)| q + Vec2::new(x - 8.0, y - 8.0))
                .collect();
            painter.add(egui::Shape::line(pts, Stroke::new(1.2, Color32::WHITE)));
        }
        // the same corner picker the photo grid has
        let corner = Rect::from_min_size(cell.left_top() + Vec2::new(6.0, 6.0), Vec2::splat(20.0));
        painter.circle_filled(
            corner.center(),
            9.0,
            if selected { YELLOW } else { Color32::from_black_alpha(90) },
        );
        if selected {
            let q = corner.center();
            painter.line_segment([q + Vec2::new(-5.0, 0.0), q + Vec2::new(-1.5, 4.0)], Stroke::new(2.5, INK));
            painter.line_segment([q + Vec2::new(-1.5, 4.0), q + Vec2::new(4.5, -4.0)], Stroke::new(2.5, INK));
        }
        // name + count strip along the bottom
        let strip = Rect::from_min_max(
            Pos2::new(cell.left(), cell.bottom() - 34.0),
            cell.right_bottom(),
        );
        painter.rect_filled(strip, 0.0, Color32::from_black_alpha(170));
        painter.text(
            Pos2::new(strip.left() + 8.0, strip.top() + 4.0),
            egui::Align2::LEFT_TOP,
            truncate(&a.title, 20),
            egui::FontId::proportional(14.0),
            Color32::WHITE,
        );
        painter.text(
            Pos2::new(strip.left() + 8.0, strip.bottom() - 4.0),
            egui::Align2::LEFT_BOTTOM,
            format!("{} items", a.count),
            egui::FontId::proportional(11.0),
            Color32::from_white_alpha(200),
        );
        if resp.clicked() {
            let mods = ui.ctx().input(|i| i.modifiers);
            let corner_click = resp
                .interact_pointer_pos()
                .map(|p| corner.contains(p))
                .unwrap_or(false);
            if mods.ctrl || mods.command || corner_click {
                if !self.album_selected.remove(&a.key) {
                    self.album_selected.insert(a.key.clone());
                }
            } else {
                self.fetch_lib(format!("album:{}", a.key));
            }
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        resp.context_menu(|ui| {
            if std::env::var("GPHOTOS_CTXDBG").is_ok() {
                eprintln!("[CTX] album menu opened");
            }
            let web = format!("https://photos.google.com/album/{}", a.key);
            if ui.button("Open on web").clicked() {
                open_url(&web);
                ui.close_menu();
            }
            if ui.button("Copy link").clicked() {
                let _ = copy_text_to_clipboard(&web);
                tx.send(Msg::Toast {
                    msg: "album link copied".into(),
                    is_error: false,
                })
                .ok();
                ui.close_menu();
            }
            if ui.button("Share album").clicked() {
                self.album_selected.clear();
                self.album_selected.insert(akey.clone());
                self.album_action("share");
                ui.close_menu();
            }
            if ui.button("Manage").clicked() {
                // Google exposes no public RPC for changing link access; the
                // sharing settings live on the album's web page
                open_url(&web);
                tx.send(Msg::Toast {
                    msg: "opened album sharing settings on the web".into(),
                    is_error: false,
                })
                .ok();
                ui.close_menu();
            }
        });
    }


    // one SystemTime::now() per day, not per photo per frame
    fn cached_date_label(&mut self, ts: i64) -> std::sync::Arc<str> {
        let day = ts.div_euclid(86400);
        if let Some(s) = self.date_labels.get(&day) {
            return s.clone(); // one atomic increment instead of a heap allocation
        }
        let s: std::sync::Arc<str> = date_label(ts).into();
        self.date_labels.insert(day, s.clone());
        s
    }
    // ---- storage hogs: which photos/videos eat the quota ---------------------

    // one full-library measurement, used by the sidebar summary and the
    // biggest-items page alike — the account is read over ~170 rpcs, once
    fn scan_storage(&self, to_hogs: bool) {
        if self.scan_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let sess_sh = self.lib_session.clone();
        let sizes_sh = self.sizes.clone();
        let charged_sh = self.charged.clone();
        let items_sh = self.all_items.clone();
        let bulk_sh = self.bulk.clone();
        let sel = self.selected.clone();
        let epoch_sh = self.account_epoch.clone();
        let epoch = epoch_sh.load(Ordering::Relaxed);
        let tx = self.tx.clone();
        let grid_items_sh = self.lib_items.clone();
        thread::spawn(move || {
            // snapshot whether a grid is already visible when the scan starts
            let grid_empty_sh = Arc::new(Mutex::new(grid_items_sh.lock().unwrap().is_empty()));
            let Some(s) = sess_sh.lock().unwrap().clone().or_else(|| capture_selected(&sel)) else {
                tx.send(Msg::Toast {
                    msg: "no photos.google.com session — sign in first".into(),
                    is_error: true,
                })
                .ok();
                return;
            };
            let rows = library::scan_all(&s);
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                return; // account switched while we were measuring
            }
            let st = library::storage_stats(&rows);
            let pairs: Vec<(String, u64)> = rows
                .iter()
                .filter(|r| r.info.size > 0)
                .map(|r| (r.item.media_key.clone(), r.info.size))
                .collect();
            {
                let mut m = sizes_sh.lock().unwrap();
                for (k, v) in &pairs {
                    m.insert(k.clone(), *v);
                }
                *charged_sh.lock().unwrap() = rows
                    .iter()
                    .filter(|r| r.info.takes_up_space == Some(true))
                    .map(|r| r.item.media_key.clone())
                    .collect();
            }
            let sizes = sizes_sh.lock().unwrap().clone();
            let mut sorted: Vec<library::MediaItem> = rows.iter().map(|r| r.item.clone()).collect();
            sorted.sort_by(|a, b| {
                sizes
                    .get(&b.media_key)
                    .copied()
                    .unwrap_or(0)
                    .cmp(&sizes.get(&a.media_key).copied().unwrap_or(0))
            });
            let bulk_map: std::collections::HashMap<String, (String, String)> = rows
                .iter()
                .map(|r| (r.item.media_key.clone(), (r.info.filename.clone(), r.info.description.clone())))
                .collect();
            bulk_sh.lock().unwrap().clone_from(&bulk_map);
            tx.send(Msg::BulkInfo(bulk_map)).ok();
            tx.send(Msg::Sizes(pairs)).ok();
            tx.send(Msg::StorageStats(Box::new(st))).ok();
            let mut timeline: Vec<library::MediaItem> = rows
                .iter()
                .filter(|r| r.list == "timeline")
                .map(|r| r.item.clone())
                .collect();
            timeline.sort_by(|a, b| b.ts.cmp(&a.ts));
            // The scan is for storage statistics, not a grid refresh. Publishing
            // its full-timeline result here makes the visible grid flicker
            // ("recent: loading" -> items -> loading) on every launch, because
            // it races the normal paged fetch. Only use it when nothing loaded.
            if !to_hogs && grid_empty_sh.lock().unwrap().clone() {
                tx.send(Msg::Lib(timeline.clone(), "recent".into(), None)).ok();
            }
            tx.send(Msg::AllItems(sorted.clone())).ok();
            if to_hogs {
                tx.send(Msg::Lib(sorted.clone(), "hogs".into(), None)).ok();
                let jobs: Vec<(String, String)> = sorted
                    .iter()
                    .take(60)
                    .map(|i| (i.media_key.clone(), i.thumb.clone()))
                    .filter(|(_, u)| !u.is_empty())
                    .collect();
                fetch_thumbs_for(&s, &jobs, &tx);
            }
            items_sh.lock().unwrap().clone_from(&sorted);
        });
    }

    fn fetch_hogs(&mut self) {
        if self.lib_busy.swap(true, Ordering::Relaxed) {
            return;
        }
        *self.lib_view.lock().unwrap() = "hogs".into();
        self.lib_next.lock().unwrap().take();
        self.lib_selected.lock().unwrap().clear();
        let all = self.all_items.lock().unwrap().clone();
        if all.is_empty() || self.hog_stats.is_none() {
            self.lib_busy.store(false, Ordering::Relaxed);
            self.scan_storage(true);
            self.spawn_action(move |tx| {
                tx.send(Msg::Toast {
                    msg: "measuring what eats your storage… (a minute or two)".into(),
                    is_error: false,
                })
                .ok();
            });
            return;
        }
        let sizes = self.sizes.lock().unwrap().clone();
        let mut sorted = all;
        sorted.sort_by(|a, b| {
            sizes
                .get(&b.media_key)
                .copied()
                .unwrap_or(0)
                .cmp(&sizes.get(&a.media_key).copied().unwrap_or(0))
        });
        *self.lib_items.lock().unwrap() = sorted.clone();
        self.items_gen.fetch_add(1, Ordering::Relaxed);
        self.lib_busy.store(false, Ordering::Relaxed);
        let tx = self.tx.clone();
        tx.send(Msg::Lib(sorted.clone(), "hogs".into(), None)).ok();
        let session = self.lib_session.lock().unwrap().clone();
        if let Some(s) = session {
            let jobs: Vec<(String, String)> = sorted
                .iter()
                .take(60)
                .map(|i| (i.media_key.clone(), i.thumb.clone()))
                .filter(|(_, u)| !u.is_empty())
                .collect();
            fetch_thumbs_for(&s, &jobs, &tx);
        }
    }

    // ---- full-screen photo viewer (web/app controls, native) -----------------

    fn open_viewer(&mut self, key: &str) {
        self.slideshow_at = Instant::now();
        self.video_stop();
        self.viewer = Some(key.to_string());
        self.viewer_zoom = 1.0;
        self.viewer_info_open = false;
        self.viewer_confirm_del = false;
        self.viewer_desc.clear();
        self.viewer_date.clear();
        self.caption_loaded_for = None;
        *self.viewer_info.lock().unwrap() = None;
        *self.viewer_ext.lock().unwrap() = None;
        // instant preview from the grid thumb while the full image loads
        if std::env::var("GPHOTOS_INFO").is_ok() {
            self.viewer_info_open = true;
        }
        let preview = self.thumbs.lock().unwrap().get(key).cloned();
        *self.viewer_img.lock().unwrap() = preview.map(|p| (key.to_string(), p));
        // drop any full-size textures from previous photos: each is up to
        // ~22 MB, and leaving them behind ballooned RAM on fast navigation
        let old_keys: Vec<String> = self.tex.keys()
            .filter(|k| k.starts_with("full:") && k.as_str() != format!("full:{}", key))
            .cloned().collect();
        for k in old_keys { self.tex.remove(&k); }
        let it = self
            .lib_items
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.media_key == key)
            .cloned();
        let Some(it) = it else { return };
        // videos start playing straight away, like the phone app
        if it.is_video {
            self.open_video(&it);
        } else {
            self.prewarm_stop();
        }
        let sess_sh = self.lib_session.clone();
        let img_sh = self.viewer_img.clone();
        let tx = self.tx.clone();
        let key2 = key.to_string();
        thread::spawn(move || {
            let dir = full_cache_dir();
            let _ = fs::create_dir_all(&dir);
            let path = dir.join(&key2);
            // =w2048 render, disk-cached like the thumbs
            let bytes = match fs::read(&path) {
                Ok(b) => Some(b),
                Err(_) => fetch_media(
                    &it.thumb,
                    &library::fife_cookie_header(),
                    "=w2048",
                )
                .map(|b| {
                    if cache_on() {
                        let _ = fs::write(&path, &b);
                    }
                    b
                }),
            };
            if let Some(b) = bytes {
                if let Some(img) = image::load_from_memory(&b).ok().map(|i| i.to_rgba8()) {
                    let (w, h) = img.dimensions();
                    let ci = egui::ColorImage::from_rgba_unmultiplied(
                        [w as usize, h as usize],
                        img.as_raw(),
                    );
                    let _ = tx.send(Msg::Viewer(key2.clone(), ci));
                }
            }
            // filename / size / description metadata
            if let Some(s) = sess_sh.lock().unwrap().clone() {
                if let Some(info) =
                    library::bulk_info(&s, &[it.media_key.clone()]).into_iter().next()
                {
                    let _ = tx.send(Msg::ViewerInfo(info));
                }
                if let Some(ext) = library::item_ext(&s, &it.media_key) {
                    let _ = tx.send(Msg::ViewerExt(key2.clone(), ext));
                }
            }
        });
    }

    fn close_viewer(&mut self) {
        if self.fullscreen {
            self.fullscreen = false;
            self.restore_window = true;
        }
        // you closed a photo: land back on it — but only if it is still in the
        // grid; a trashed/archived item would strand a key that matches a
        // different photo in a later view
        let closing = self.viewer.take();
        // release the full-size GPU texture and decoded image on close
        if let Some(k) = &closing {
            self.tex.remove(&format!("full:{}", k));
            *self.viewer_img.lock().unwrap() = None;
        }
        if closing
            .as_ref()
            .map(|k| self.lib_items.lock().unwrap().iter().any(|i| &i.media_key == k))
            .unwrap_or(false)
        {
            self.scroll_anchor = closing;
        }
        self.viewer_confirm_del = false;
        self.remember_position();
        save_resume(&self.resume);
        self.video_stop();
        self.prewarm_stop();
    }

    // ---- native video playback ------------------------------------------------

    // resolve the signed download url once; playback itself is local decoding
    // resolve the fife redirect once per item; the answer stays valid a while
    fn video_url_cached(&self, key: &str) -> Option<String> {
        let map = self.vid_urls.lock().unwrap();
        map.get(key)
            .filter(|(u, at)| !u.is_empty() && at.elapsed() < Duration::from_secs(1500))
            .map(|(u, _)| u.clone())
    }

    fn resolve_video_url(&self, key: &str, thumb: &str) {
        if !self.resolving.lock().unwrap().insert(key.to_string()) {
            return;
        }
        let tx = self.tx.clone();
        let (key, target) = (key.to_string(), sized_url(thumb, "=dv"));
        let resolving = self.resolving.clone();
        let non_seekable = self.non_seekable.clone();
        thread::spawn(move || {
            let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
            let url = library::curl_with_cookie(
                &[
                    format!("url = {}", library::curl_quote(&target)),
                    format!("output = {}", library::curl_quote(null_device(cfg!(windows)))),
                    "write-out = \"%{redirect_url}\"".into(),
                    "max-time = 30".into(),
                    format!("user-agent = {}", library::curl_quote(ua)),
                ],
                &library::fife_cookie_header(),
            )
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default();
            if !url.is_empty() {
                let probe = library::curl_with_cookie(
                    &[
                        format!("url = {}", library::curl_quote(&url)),
                        format!("output = {}", library::curl_quote(null_device(cfg!(windows)))),
                        "write-out = \"%{http_code}\"".into(),
                        "range = \"0-0\"".into(),
                        "max-time = 8".into(),
                        format!("user-agent = {}", library::curl_quote(ua)),
                    ],
                    &library::fife_cookie_header(),
                )
                .map(|b| String::from_utf8_lossy(&b).trim().to_string())
                .unwrap_or_default();
                if probe != "206" {
                    if std::env::var("GPHOTOS_VDBG").is_ok() {
                        eprintln!("cdn range refused for {}: {}", key, probe);
                    }
                    non_seekable.lock().unwrap().insert(key.clone());
                }
            }
            resolving.lock().unwrap().remove(&key);
            let _ = tx.send(Msg::VideoUrl(key, url));
        });
    }

    // pointer rests on a video tile: get its decoder going before the click
    // pointer resting on a video tile: 350 ms later it plays, muted, in place
    fn hover_tick(&mut self, ctx: &egui::Context) {
        let want = self.hover_want.take();
        let key_now = want.as_ref().map(|(k, _)| k.clone());
        if let Some(hv) = self.hover_vid.as_ref() {
            if Some(&hv.key) != key_now.as_ref() {
                hv.stop();
                self.hover_vid = None;
            }
        }
        let Some((key, thumb)) = want else {
            self.hover_prev = None;
            return;
        };
        if self.hover_prev.as_ref() != Some(&key) {
            self.hover_prev = Some(key.clone());
            self.hover_since = Instant::now();
            return;
        }
        if self.hover_vid.is_none() && self.hover_since.elapsed() > Duration::from_millis(350) {
            let dims = self
                .lib_items
                .lock()
                .unwrap()
                .iter()
                .find(|i| i.media_key == key)
                .map(|i| (i.w, i.h))
                .unwrap_or((16, 9));
            self.hover_vid = Some(start_hover_preview(key.clone(), thumb, dims));
        }
        if let Some(hv) = self.hover_vid.as_mut() {
            let next = hv.slot.lock().unwrap().take();
            if let Some((gen, img)) = next {
                if gen != hv.last_gen {
                    hv.last_gen = gen;
                    hv.tex = Some(ctx.load_texture(
                        "hover_preview",
                        img,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
        }
    }

    fn prewarm_tick(&mut self, it: &library::MediaItem) {
        if self.prewarm.as_ref().map(|p| p.key == it.media_key).unwrap_or(false) {
            return;
        }
        // a decoder that hit the end (or died) is finished: reap it
        if self
            .prewarm
            .as_ref()
            .map(|p| p.slots.eof.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            self.prewarm_stop();
        }
        let Some(url) = self.video_url_cached(&it.media_key) else {
            self.resolve_video_url(&it.media_key, &it.thumb);
            return;
        };
        self.prewarm_stop();
        let (w, h) = decode_size((it.w, it.h));
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss", "0"])
            .arg("-i")
            .arg(&url)
            .args(["-an", "-r", "25"])
            .args(["-vf", &format!("scale={}:{}", w, h)])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else { return };
        let Some(out) = child.stdout.take() else { return };
        let slots = Slots::new();
        let gen = slots.gen.load(Ordering::Relaxed);
        spawn_reader(out, w, h, slots.clone(), gen, (Instant::now(), 25.0));
        self.prewarm = Some(Prewarm {
            key: it.media_key.clone(),
            url,
            child: Arc::new(Mutex::new(Some(child))),
            slots,
            w,
            h,
        });
    }

    fn prewarm_stop(&mut self) {
        if let Some(p) = self.prewarm.take() {
            p.slots.gen.fetch_add(1, Ordering::Relaxed);
            if let Some(mut c) = p.child.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    fn open_video(&mut self, it: &library::MediaItem) {
        self.ctl_move = Instant::now();
        let key = it.media_key.clone();
        let thumb = it.thumb.clone();
        let dur = it.duration_ms as f64 / 1000.0;
        // reuse an already resolved url for this item
        if let Some(v) = self.vid.as_mut() {
            if v.key == key && !v.url.is_empty() {
                if v.ended {
                    self.video_start(0.0);
                } else if !v.playing {
                    let at = v.pos();
                    self.video_start(at);
                }
                return;
            }
        }
        // a film with a saved position must start there, so the waiting decoder
        // (which is at zero) is not adopted
        if self.resume_at(&key, dur).is_some() {
            self.prewarm_stop();
        }
        // the tile under the pointer may already be decoding: take it over as is
        if let Some(p) = self.prewarm.take() {
            if p.key == it.media_key && !p.slots.eof.load(Ordering::Relaxed) {
                let mut v = Vid::new(key.clone(), p.url, dur, (it.w, it.h));
                v.muted = self.want_muted();
                v.child = p.child;
                v.frames = p.slots.frames;
                v.eof = p.slots.eof;
                v.frame = p.slots.frame;
                v.gen = p.slots.gen;
                v.aplay = p.slots.aplay.clone();
                v.audio_epoch = p.slots.audio_epoch.clone();
                v.size = (p.w, p.h);
                v.playing = true;
                // the hover decoder has been running in real time: adopt its
                // position and restart the clock from here
                v.frames_base = v.frames.load(Ordering::Relaxed);
                v.offset = v.frames_base as f64 / v.fps;
                v.t0 = Some(Instant::now());
                v.seeking = false;
                let at = format!("{:.3}", v.pos());
                if !v.muted {
                    let input = cached_video(&v.key)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| v.url.clone());
                    spawn_audio(&mut v, &input, &at);
                }
                self.vid = Some(v);
                self.apply_video_prefs();
                return;
            }
            p.slots.gen.fetch_add(1, Ordering::Relaxed);
            if let Some(mut c) = p.child.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        self.video_stop();
        match self.video_url_cached(&key) {
            Some(url) => {
                self.vid = Some(Vid::new(key.clone(), url, dur, (it.w, it.h)));
                self.vid.as_mut().unwrap().muted = self.want_muted();
                // pick up where this film was left, like a real player does
                let from = self.resume_at(&key, dur).unwrap_or(0.0);
                if from > 0.0 {
                    self.resume_note = Some((key.clone(), from, Instant::now()));
                }
                self.video_start(from);
            }
            None => {
                self.resolve_video_url(&key, &thumb);
                self.vid = Some(Vid::new(key, String::new(), dur, (it.w, it.h)));
                self.vid.as_mut().unwrap().muted = self.want_muted();
            }
        }
        self.apply_video_prefs();
    }

    // preferences that apply to every newly opened film
    fn apply_video_prefs(&mut self) {
        let pause = if let Some(v) = self.vid.as_mut() {
            v.looping = self.loop_default;
            v.playing && !self.autoplay
        } else {
            false
        };
        if pause {
            self.video_pause();
        }
    }

    // (re)spawn the decoders at `from`; the gen bump retires stale readers
    fn video_start(&mut self, from: f64) {
        let Some(v) = self.vid.as_mut() else { return };
        if v.url.is_empty() {
            return;
        }
        kill_vid_children(v);
        let gen = v.gen.fetch_add(1, Ordering::Relaxed) + 1;
        v.aplay.store(0, Ordering::Relaxed);
        v.audio_base = 0.0;
        v.offset = from.max(0.0);
        v.frames.store(0, Ordering::Relaxed);
        v.frames_base = 0;
        v.t0 = Some(Instant::now());
        v.seeking = true;
        v.eof.store(false, Ordering::Relaxed);
        v.preview = None;
        v.ended = false;
        v.playing = true;
        let (w, h) = decode_size(v.src);
        let at = format!("{:.3}", v.offset);
        // a downloaded copy turns every seek from a 2 s stream reopen into a
        // local disk seek
        let cached = if std::env::var("GPHOTOS_FORCE_STREAM").is_ok() {
            None
        } else {
            cached_video(&v.key)
        };
        if std::env::var("GPHOTOS_VDBG").is_ok() {
            eprintln!("vstart from={:.2} gen={} streamed={}", from, gen, cached.is_none());
        }
        let input = cached
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| v.url.clone());
        let streamed = cached.is_none();
        if streamed {
            // never leave the previous frame on screen while a remote seek
            // finds its first frame: show the thumbnail + buffering instead
            v.tex = None;
            v.seeking = true;
        }
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if streamed {
            // bound a hung range request and let ffmpeg read enough of a
            // high-bitrate file to find the moov/codec index
            cmd.args([
                "-rw_timeout", "15000000",
                "-probesize", "100000000",
                "-analyzeduration", "10000000",
            ]);
        }
        cmd.args(["-ss", &at])
            .arg("-i")
            .arg(&input)
            .args(["-an", "-r", "25"])
            .args(["-vf", &format!("scale={}:{}", w, h)])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else {
            v.playing = false;
            return;
        };
        let Some(mut out) = child.stdout.take() else {
            return;
        };
        *v.child.lock().unwrap() = Some(child);
        {
            let (gen_sh, slot, frames) = (v.gen.clone(), v.frame.clone(), v.frames.clone());
            let (at2, input2) = (at.clone(), input.clone());
            thread::spawn(move || {
                let mut snap = Command::new("ffmpeg");
                snap.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
                if streamed {
                    snap.args(["-rw_timeout", "15000000", "-probesize", "100000000"]);
                }
                let Ok(out) = snap
                    .args(["-ss", &at2])
                    .arg("-i")
                    .arg(&input2)
                    .args(["-frames:v", "1", "-an"])
                    .args(["-vf", &format!("scale={}:{}", w, h)])
                    .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .and_then(|c| c.wait_with_output())
                else {
                    return;
                };
                if out.stdout.len() < w * h * 4 {
                    return;
                }
                // a real frame may have landed meanwhile: never step backwards
                if gen_sh.load(Ordering::Relaxed) != gen || frames.load(Ordering::Relaxed) > 0 {
                    return;
                }
                *slot.lock().unwrap() =
                    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &out.stdout));
            });
        }
        if !v.muted {
            let input = cached_video(&v.key)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| v.url.clone());
            spawn_audio(v, &input, &at);
        }
        let slots = Slots {
            frames: v.frames.clone(),
            eof: v.eof.clone(),
            frame: v.frame.clone(),
            gen: v.gen.clone(),
            aplay: v.aplay.clone(),
            audio_epoch: v.audio_epoch.clone(),
        };
        spawn_reader(out, w, h, slots, gen, (Instant::now(), 25.0));
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }

    fn video_stop(&mut self) {
        self.seek_thumb = None;
        self.resume_note = None;
        self.pending_seek = None;
        self.pending_since = None;
        if let Some(v) = self.vid.as_ref() {
            let at = v.pos();
            if self.resume_ok && Self::resume_allowed(at, v.dur) {
                self.resume.insert(v.key.clone(), at);
            }
        }
        if let Some(mut v) = self.vid.take() {
            v.gen.fetch_add(1, Ordering::Relaxed);
            kill_vid_children(&mut v);
        }
    }

    fn video_pause(&mut self) {
        let Some(v) = self.vid.as_mut() else { return };
        let at = v.pos();
        if self.resume_ok && Self::resume_allowed(at, v.dur) {
            self.resume.insert(v.key.clone(), at);
            self.resume_wrote = Instant::now();
        }
        v.gen.fetch_add(1, Ordering::Relaxed);
        kill_vid_children(v);
        v.playing = false;
        v.offset = if v.dur > 0.0 { at.min(v.dur) } else { at };
        v.frames_base = v.frames.load(Ordering::Relaxed);
        v.t0 = None;
        v.seeking = false;
        v.preview = None;
    }

    fn video_toggle(&mut self) {
        self.ctl_move = Instant::now();
        let (playing, at, ended) = match self.vid.as_ref() {
            Some(v) => (v.playing, v.pos(), v.ended),
            None => return,
        };
        if playing {
            self.video_pause();
        } else if ended {
            self.video_start(0.0);
        } else {
            self.video_start(at);
        }
    }

    fn video_mute(&mut self) {
        let Some(v) = self.vid.as_ref() else { return };
        let at = v.pos();
        let was_playing = v.playing;
        let muted = !v.muted;
        if let Some(v) = self.vid.as_mut() {
            v.muted = muted;
        }
        // A fresh segment is the simplest way to keep the frame clock, the
        // audio counter and the wall clock aligned after a clock change.
        self.video_start(at);
        if !was_playing {
            self.video_pause();
        }
    }

    fn video_seek_to(&mut self, t: f64) {
        let Some(v) = self.vid.as_ref() else { return };
        let dur = v.dur;
        let playing = v.playing;
        let ended = v.ended;
        let t = t.clamp(0.0, if dur > 0.0 { dur } else { t.max(0.0) });
        let vkey = v.key.clone();
        if self.non_seekable.lock().unwrap().contains(&vkey)
            && cached_video(&vkey).is_none()
            && cache_on()
        {
            let url = v.url.clone();
            cache_video_async(&vkey, &url);
            let v = self.vid.as_mut().unwrap();
            kill_vid_children(v);
            v.tex = None;
            v.seeking = true;
            v.playing = false;
            v.offset = t;
            v.preview = Some(t);
            self.pending_seek = Some((vkey, t, v.gen.load(Ordering::Relaxed)));
            self.pending_since = Some(Instant::now());
            return;
        }
        if playing || ended {
            self.video_start(t);
            return;
        }
        let Some(v) = self.vid.as_mut() else { return };
        v.offset = t;
        v.frames.store(0, Ordering::Relaxed);
        v.frames_base = 0;
        v.t0 = None;
        v.seeking = true;
        v.preview = None;
        let url = cached_video(&v.key)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| v.url.clone());
        let (slot, gen_sh) = (v.frame.clone(), v.gen.clone());
        let gen = gen_sh.fetch_add(1, Ordering::Relaxed) + 1;
        let (w, h) = decode_size(v.src);
        let at = format!("{:.3}", t);
        thread::spawn(move || {
            let mut snap = Command::new("ffmpeg");
            snap.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
            if url.starts_with("http") {
                snap.args(["-rw_timeout", "15000000", "-probesize", "100000000"]);
            }
            let Ok(out) = snap
                .args(["-ss", &at])
                .arg("-i")
                .arg(&url)
                .args(["-frames:v", "1", "-an"])
                .args(["-vf", &format!("scale={}:{}", w, h)])
                .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .and_then(|c| c.wait_with_output())
            else {
                return;
            };
            if out.stdout.len() < w * h * 4 {
                return;
            }
            if gen_sh.load(Ordering::Relaxed) != gen {
                return;
            }
            *slot.lock().unwrap() =
                Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &out.stdout));
        });
    }

    // where this file was left off, if it was left mid-film
    // a film is worth resuming only if it is long and was left mid-way; ten
    // seconds from the end counts as finished, like mpv and every set-top box
    fn resume_allowed(at: f64, dur: f64) -> bool {
        dur >= 40.0 && at > 5.0 && at < dur - 10.0
    }

    // muted when the user asked for it, and always under the test hook
    fn want_muted(&self) -> bool {
        std::env::var("GPHOTOS_MUTE").is_ok() || !self.video_sound
    }

    fn resume_at(&self, key: &str, dur: f64) -> Option<f64> {
        if !self.resume_ok || dur < 40.0 {
            return None;
        }
        let at = *self.resume.get(key)?;
        Self::resume_allowed(at, dur).then_some(at)
    }

    fn remember_position(&mut self) {
        let (key, pos, dur, playing) = match self.vid.as_ref() {
            Some(v) => (v.key.clone(), v.pos(), v.dur, v.playing && !v.ended),
            None => return,
        };
        if !self.resume_ok || !playing {
            return;
        }
        if Self::resume_allowed(pos, dur) {
            self.resume.insert(key, pos);
        } else {
            self.resume.remove(&key);
        }
        if self.resume_wrote.elapsed() > Duration::from_secs(10) {
            self.resume_wrote = Instant::now();
            save_resume(&self.resume);
        }
    }

    // one still per hovered second, built only while the pointer is on the bar
    fn seek_thumb_ask(&self, key: &str, input: &str, at: f64, dims: (u32, u32)) -> Option<SeekThumb> {
        if input.is_empty() {
            return None;
        }
        if let Some(t) = self.seek_thumb.as_ref() {
            if t.key == key && (t.busy.load(Ordering::Relaxed) || (t.at - at).abs() < 2.0) {
                return None;
            }
        }
        let (w, h) = if dims.0 == 0 || dims.1 == 0 { (16, 9) } else { dims };
        let pw = 176usize;
        let ph = (((pw as f64 * h as f64 / w as f64) / 2.0).round() as usize * 2).clamp(64, 320);
        let slot = Arc::new(Mutex::new(None));
        let busy = Arc::new(AtomicBool::new(true));
        let (slot2, busy2) = (slot.clone(), busy.clone());
        let input = input.to_string();
        thread::spawn(move || {
            let out = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
                .args(["-ss", &format!("{:.3}", at)])
                .arg("-i")
                .arg(&input)
                .args(["-frames:v", "1", "-an"])
                .args(["-vf", &format!("scale={}:{}", pw, ph)])
                .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .and_then(|c| c.wait_with_output());
            if let Ok(o) = out {
                if o.stdout.len() >= pw * ph * 4 {
                    *slot2.lock().unwrap() = Some(egui::ColorImage::from_rgba_unmultiplied(
                        [pw, ph],
                        &o.stdout,
                    ));
                }
            }
            busy2.store(false, Ordering::Relaxed);
        });
        Some(SeekThumb {
            key: key.to_string(),
            at,
            tex: None,
            slot,
            busy,
        })
    }

    fn seek_thumb_tick(&mut self, ctx: &egui::Context) {
        let Some(t) = self.seek_thumb.as_mut() else { return };
        let Some(img) = t.slot.lock().unwrap().take() else { return };
        t.tex = Some(ctx.load_texture("seek_thumb", img, egui::TextureOptions::LINEAR));
    }

    fn video_seek_rel(&mut self, d: f64) {
        let pos = match self.vid.as_ref() {
            Some(v) => v.pos(),
            None => return,
        };
        self.video_seek_to(pos + d);
    }

    // end of file: stop at the last frame, or wrap around when looping
    fn video_tick(&mut self) {
        if let Some((key, t, gen)) = self.pending_seek.clone() {
            let ready = cached_video(&key).is_some();
            let stale = self
                .pending_since
                .map(|s| s.elapsed() > Duration::from_secs(600))
                .unwrap_or(false);
            let same = self
                .vid
                .as_ref()
                .map(|v| v.gen.load(Ordering::Relaxed) == gen)
                .unwrap_or(false);
            if ready && same {
                self.pending_seek = None;
                self.pending_since = None;
                self.video_start(t); // now picks the local file
                return;
            }
            if stale || !same {
                // download failed or the viewer moved on: try the remote seek
                self.pending_seek = None;
                self.pending_since = None;
                if same {
                    self.video_start(t);
                    return;
                }
            }
        }
        self.remember_position();
        let (eof, playing, looping) = match self.vid.as_ref() {
            Some(v) => (v.eof.load(Ordering::Relaxed), v.playing, v.looping),
            None => return,
        };
        if !eof || !playing {
            return;
        }
        if looping {
            self.video_start(0.0);
        } else {
            self.video_pause();
            if let Some(v) = self.vid.as_mut() {
                v.ended = true;
                v.frames.store(0, Ordering::Relaxed);
                v.offset = v.dur.max(0.0);
            }
        }
    }

    // the in-viewer player: picture, plus one centred control pill
    fn video_center_ui(&mut self, ui: &mut egui::Ui, pic: Rect, ctl: Rect) {
        let ctx = ui.ctx().clone();
        let hover_pos = ctx.input(|i| i.pointer.hover_pos());
        // test hook: GPHOTOS_HOVERBAR=0.35 hovers the bar without a pointer
        let fake_hover = std::env::var("GPHOTOS_HOVERBAR")
            .ok()
            .and_then(|v| v.parse::<f32>().ok());
        let mut act: Option<(u8, f64)> = None;
        // cloned before the player is borrowed mutably: the hover still, if one
        // has landed, and the request this frame produces
        let thumb_live: Option<egui::TextureHandle> =
            self.seek_thumb.as_ref().and_then(|t| t.tex.clone());
        let mut want_thumb: Option<(String, f64, String)> = None;
        let mut drop_thumb = false;
        let fav_here = self
            .lib_items
            .lock()
            .unwrap()
            .iter()
            .find(|x| Some(&x.media_key) == self.viewer.as_ref())
            .map(|x| x.is_favorite)
            .unwrap_or(false);
        let fullscreen = self.fullscreen;
        let info_open = self.viewer_info_open;
        let cv = self.current_lib_view();
        let v_trash = cv == "trash";
        let v_archive = cv == "archive";
        let v_locked = cv == "locked";
        {
            let Some(v) = self.vid.as_mut() else { return };
            if let Some(img) = v.frame.lock().unwrap().take() {
                if v.frames.load(Ordering::Relaxed) > v.frames_base {
                    v.seeking = false;
                }
                let sz = img.size;
                match &mut v.tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => {
                        v.tex = Some(ctx.load_texture(
                            format!("vid:{}", v.key),
                            img,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
                v.size = (sz[0], sz[1]);
            }
            let has_frame = v.tex.is_some();
            if let Some(t) = &v.tex {
                let tw = (t.size()[0] as f32).max(1.0);
                let th = (t.size()[1] as f32).max(1.0);
                let fit = (pic.width() / tw).min(pic.height() / th);
                let size = Vec2::new(tw * fit, th * fit);
                let pos = pic.center() - size * 0.5;
                ui.painter().image(
                    t.id(),
                    Rect::from_min_size(pos, size),
                    Rect::from_min_size(Pos2::ZERO, Vec2::splat(1.0)),
                    Color32::WHITE,
                );
            }
            let pos_now = v.pos();
            let dur = v.dur;
            let playing = v.playing;
            let ended = v.ended;
            let muted = v.muted;
            let looping = v.looping;
            let url_empty = v.url.is_empty();

            // the black bars beside the picture are prev/next zones, as in a
            // gallery; clicking the picture itself still toggles playback
            let hit = pic;
            let pic_resp = ui.interact(hit, egui::Id::new("vid_pic"), egui::Sense::click());
            let (vw, vh) = v.size;
            let vf = (pic.width() / vw.max(1) as f32).min(pic.height() / vh.max(1) as f32);
            let vdisp = Vec2::new(vw.max(1) as f32 * vf, vh.max(1) as f32 * vf);
            let vshown = Rect::from_center_size(pic.center(), vdisp);
            if pic_resp.double_clicked() {
                act = Some((7, 0.0));
            } else if pic_resp.clicked() {
                act = match pic_resp.interact_pointer_pos() {
                    Some(p) if p.x < vshown.left() => Some((15, 0.0)),
                    Some(p) if p.x > vshown.right() => Some((16, 0.0)),
                    _ => Some((0, 0.0)),
                };
            }
            // states over the picture
            if !has_frame {
                // the tile's own thumb, dimmed, so the window is never blank
                if let Some(post) = self.tex.get(&v.key) {
                    let tw = (post.size()[0] as f32).max(1.0);
                    let th = (post.size()[1] as f32).max(1.0);
                    let fit = (pic.width() / tw).min(pic.height() / th);
                    let size = Vec2::new(tw, th) * fit;
                    let r = Rect::from_min_size(pic.center() - size * 0.5, size);
                    ui.painter().image(
                        post.id(),
                        r,
                        Rect::from_min_size(Pos2::ZERO, Vec2::splat(1.0)),
                        Color32::from_gray(150),
                    );
                }
                let msg = if ended {
                    "could not load video — click to retry"
                } else if url_empty {
                    "opening video…"
                } else if self.pending_seek.is_some() {
                    "downloading for a seek…"
                } else {
                    "buffering…"
                };
                ui.painter().text(
                    pic.center(),
                    Align2::CENTER_CENTER,
                    msg,
                    egui::FontId::proportional(16.0),
                    Color32::WHITE,
                );
            } else if ended {
                let c = pic.center();
                let hot = hover_pos.map(|h| h.distance(c) < 34.0).unwrap_or(false);
                ui.painter().circle_filled(
                    c,
                    27.0,
                    if hot {
                        Color32::from_white_alpha(235)
                    } else {
                        Color32::from_white_alpha(185)
                    },
                );
                ui.painter().circle_stroke(c, 13.0, Stroke::new(2.0, INK));
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        c + Vec2::new(-3.0, -6.0),
                        c + Vec2::new(7.0, 0.0),
                        c + Vec2::new(-3.0, 6.0),
                    ],
                    INK,
                    Stroke::NONE,
                ));
                if ui
                    .interact(
                        Rect::from_center_size(c, Vec2::splat(66.0)),
                        egui::Id::new("vid_replay"),
                        egui::Sense::click(),
                    )
                    .clicked()
                {
                    act = Some((4, 0.0));
                }
            } else if !playing {
                // paused: a small play hint, like youtube
                let c = pic.center();
                ui.painter().circle_filled(c, 25.0, Color32::from_black_alpha(150));
                ui.painter()
                    .circle_stroke(c, 25.0, Stroke::new(1.5, Color32::from_white_alpha(200)));
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        c + Vec2::new(-5.5, -10.0),
                        c + Vec2::new(10.0, 0.0),
                        c + Vec2::new(-5.5, 10.0),
                    ],
                    Color32::WHITE,
                    Stroke::NONE,
                ));
            }

            // "resumed from 12:34" — said out loud, and undoable with one click
            if let Some((key, at, shown)) = self.resume_note.clone() {
                if key == v.key && shown.elapsed() < Duration::from_secs(9) {
                    let text = format!("resumed from {}", hhmmss(at));
                    let font = egui::FontId::proportional(12.5);
                    let galley = ui.painter().layout_no_wrap(
                        text.clone(),
                        font.clone(),
                        Color32::WHITE,
                    );
                    let wipe = "start over";
                    let g2 = ui.painter().layout_no_wrap(
                        wipe.to_string(),
                        font,
                        YELLOW,
                    );
                    let pill = Rect::from_min_size(
                        Pos2::new(pic.left() + 14.0, pic.top() + 12.0),
                        Vec2::new(galley.size().x + g2.size().x + 34.0, 26.0),
                    );
                    let p = ui.painter();
                    p.rect_filled(pill, 13.0, Color32::from_black_alpha(190));
                    p.galley(
                        Pos2::new(pill.left() + 12.0, pill.center().y - galley.size().y * 0.5),
                        galley,
                        Color32::WHITE,
                    );
                    p.galley(
                        Pos2::new(
                            pill.right() - g2.size().x - 12.0,
                            pill.center().y - g2.size().y * 0.5,
                        ),
                        g2,
                        YELLOW,
                    );
                    if ui
                        .interact(pill, egui::Id::new("resume_note"), egui::Sense::click())
                        .clicked()
                    {
                        act = Some((4, 0.0));
                        self.resume_note = None;
                        self.resume.remove(&key);
                    }
                } else if shown.elapsed() >= Duration::from_secs(9) {
                    self.resume_note = None;
                }
            }
            // collapsed: a progress hairline at the very bottom, nothing else
            if ctl.height() < 20.0 {
                let p = ui.painter();
                p.rect_filled(ctl, 0.0, Color32::from_rgb(0x16, 0x13, 0x10));
                let f = if dur > 0.0 {
                    (pos_now / dur).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
                let line = Rect::from_center_size(ctl.center(), Vec2::new(ctl.width(), 2.0));
                p.rect_filled(line, 1.0, Color32::from_white_alpha(45));
                p.rect_filled(
                    Rect::from_min_size(line.left_top(), Vec2::new(line.width() * f, 2.0)),
                    1.0,
                    YELLOW,
                );
                if act.is_some() {
                    self.ctl_touch = Instant::now();
                }
                match act {
                    Some((0, _)) => self.video_toggle(),
                    Some((1, t)) => self.video_seek_to(t),
                    Some((2, _)) => self.video_seek_rel(-10.0),
                    Some((3, _)) => self.video_seek_rel(10.0),
                    _ => {}
                }
                return;
            }
            // ---- one transport panel: scrub row above, controls below, the
            // same block edge to edge, so the slider belongs to the bar rather
            // than floating separately. Left cluster plays, right cluster acts.
            {
                let p = ui.painter();
                p.rect_filled(ctl, 0.0, Color32::from_rgb(0x16, 0x13, 0x10));
            }
            let pad = 16.0;
            let l = ctl.left() + pad;
            let r = ctl.right() - pad;
            let y1 = ctl.top() + 16.0;
            let y2 = ctl.top() + 44.0;
            let time_w = 62.0;

            // scrub row: elapsed, track edge to edge, duration
            {
                let p = ui.painter();
                p.text(
                    Pos2::new(l, y1),
                    Align2::LEFT_CENTER,
                    hhmmss(pos_now),
                    egui::FontId::proportional(12.0),
                    Color32::from_white_alpha(230),
                );
                p.text(
                    Pos2::new(r, y1),
                    Align2::RIGHT_CENTER,
                    hhmmss(dur),
                    egui::FontId::proportional(12.0),
                    Color32::from_white_alpha(140),
                );
            }
            let track = Rect::from_min_max(
                Pos2::new((l + time_w).min(r - time_w), y1 - 2.0),
                Pos2::new((r - time_w).max(l + time_w + 60.0), y1 + 2.0),
            );
            let frac = if dur > 0.0 {
                (pos_now / dur).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            let knob_x = track.left() + track.width() * frac;
            let grab = ui.interact(
                track.expand2(Vec2::new(0.0, 12.0)),
                egui::Id::new("vid_seek"),
                egui::Sense::click_and_drag(),
            );
            {
                let p = ui.painter();
                p.rect_filled(track, 2.0, Color32::from_white_alpha(60));
                p.rect_filled(
                    Rect::from_min_size(
                        track.left_top(),
                        Vec2::new((knob_x - track.left()).max(0.0), 4.0),
                    ),
                    2.0,
                    YELLOW,
                );
                p.circle_filled(
                    Pos2::new(knob_x, y1),
                    if grab.hovered() || grab.dragged() { 6.5 } else { 4.5 },
                    Color32::WHITE,
                );
                let fake = fake_hover;
                if grab.hovered() || fake.is_some() {
                    let hp = hover_pos.or_else(|| {
                        fake.map(|f| Pos2::new(track.left() + track.width() * f, track.center().y))
                    });
                    if let Some(hp) = hp {
                        let f = ((hp.x - track.left()) / track.width()).clamp(0.0, 1.0) as f64;
                        let at = f * dur;
                        // the frame behind the bubble, when one has arrived
                        let mut label_y = ctl.top() - 14.0;
                        if let Some(t) = thumb_live {
                            let tw = (t.size()[0] as f32).max(1.0);
                            let th = (t.size()[1] as f32).max(1.0);
                            let half = tw * 0.5;
                            let x = hp.x.clamp(ctl.left() + half + 6.0, ctl.right() - half - 6.0);
                            let bottom = ctl.top() - 30.0;
                            let img = Rect::from_min_size(
                                Pos2::new(x - half, bottom - th),
                                Vec2::new(tw, th),
                            );
                            p.rect_filled(img.expand(2.0), 4.0, Color32::from_black_alpha(230));
                            p.image(
                                t.id(),
                                img,
                                Rect::from_min_size(Pos2::ZERO, Vec2::splat(1.0)),
                                Color32::WHITE,
                            );
                            label_y = bottom - th - 12.0;
                        }
                        let tip = Rect::from_center_size(
                            Pos2::new(hp.x.clamp(l, r), label_y),
                            Vec2::new(56.0, 21.0),
                        );
                        p.rect_filled(tip, 6.0, Color32::from_black_alpha(215));
                        p.text(
                            tip.center(),
                            Align2::CENTER_CENTER,
                            hhmmss(at),
                            egui::FontId::proportional(11.5),
                            Color32::WHITE,
                        );
                        if (grab.hovered() || fake.is_some()) && !grab.dragged() {
                            want_thumb = Some((v.key.clone(), at, v.url.clone()));
                        }
                    }
                }
            }
            if grab.dragged() || grab.clicked() {
                if let Some(hp) = grab.interact_pointer_pos() {
                    let f = ((hp.x - track.left()) / track.width()).clamp(0.0, 1.0) as f64;
                    v.preview = Some(f * dur);
                    if grab.clicked() {
                        act = Some((1, f * dur));
                    }
                }
            }
            if grab.drag_stopped() {
                if let Some(t) = v.preview.take() {
                    act = Some((1, t));
                }
            } else if !grab.dragged() {
                v.preview = None;
            }
            if !grab.hovered() && fake_hover.is_none() {
                drop_thumb = true;
            }
            // uosc's timeline_step: a wheel notch over the bar is five seconds
            if (grab.hovered() || fake_hover.is_some())
                && ui.ctx().input(|i| i.raw_scroll_delta.y.abs() > 0.5)
            {
                let d = if ui.ctx().input(|i| i.raw_scroll_delta.y) > 0.0 { -5.0 } else { 5.0 };
                act = Some((1, (pos_now + d).clamp(0.0, dur)));
            }

            // transport row — left: playback; right: everything else
            if icon_btn_at(
                ui,
                Pos2::new(l + 15.0, y2),
                "vid_play",
                if playing { "pause (space)" } else { "play (space)" },
                |p, c, _hot, _t| icon_play_pause(p, c, playing, true, Color32::WHITE),
            ) {
                act = Some((0, 0.0));
            }
            for (i, label) in ["-10", "+10"].iter().enumerate() {
                let r2 = Rect::from_center_size(
                    Pos2::new(l + 51.0 + 40.0 * i as f32, y2),
                    Vec2::new(36.0, 26.0),
                );
                let hot = hover_pos.map(|h| r2.contains(h)).unwrap_or(false);
                let p = ui.painter();
                if hot {
                    p.rect_filled(r2, 7.0, Color32::from_white_alpha(45));
                }
                p.text(
                    r2.center(),
                    Align2::CENTER_CENTER,
                    *label,
                    egui::FontId::proportional(11.5),
                    Color32::WHITE,
                );
                if ui
                    .interact(r2, egui::Id::new(("vid_step", i)), egui::Sense::click())
                    .clicked()
                {
                    act = Some(if i == 0 { (2, 0.0) } else { (3, 0.0) });
                }
            }
            if icon_btn_at(
                ui,
                Pos2::new(l + 132.0, y2),
                "vid_mute",
                if muted { "sound on (m)" } else { "mute (m)" },
                |p, c, hot, t| icon_speaker(p, c, hot, t, muted, playing, Color32::WHITE),
            ) {
                act = Some((5, 0.0));
            }
            if icon_btn_at(
                ui,
                Pos2::new(l + 168.0, y2),
                "vid_fav",
                if fav_here { "remove favourite" } else { "favourite" },
                |p, c, hot, t| {
                    icon_star(
                        p,
                        c,
                        hot,
                        t,
                        fav_here,
                        if fav_here { RED } else { Color32::WHITE },
                    )
                },
            ) {
                act = Some((11, 0.0));
            }
            // right cluster, anchored to the right edge
            if icon_btn_at(
                ui,
                Pos2::new(r - 15.0, y2),
                "vid_full",
                if fullscreen {
                    "leave fullscreen (f)"
                } else {
                    "fullscreen (f)"
                },
                |p, c, hot, t| icon_fullscreen(p, c, hot, t, Color32::WHITE),
            ) {
                act = Some((7, 0.0));
            }
            if icon_btn_at(
                ui,
                Pos2::new(r - 51.0, y2),
                "vid_info",
                if info_open { "hide details" } else { "details" },
                |p, c, hot, t| {
                    icon_info(
                        p,
                        c,
                        hot,
                        t,
                        if info_open { YELLOW } else { Color32::WHITE },
                    )
                },
            ) {
                act = Some((9, 0.0));
            }
            if icon_btn_at(
                ui,
                Pos2::new(r - 87.0, y2),
                "vid_save",
                "save a copy",
                |p, c, hot, t| icon_download(p, c, hot, t, Color32::WHITE),
            ) {
                act = Some((8, 0.0));
            }
            if icon_btn_at(
                ui,
                Pos2::new(r - 123.0, y2),
                "vid_trash",
                if v_trash { "restore" } else { "move to trash" },
                |p, c, hot, _t| {
                    icon_trash(
                        p,
                        c,
                        hot,
                        _t,
                        if v_trash { YELLOW } else { Color32::WHITE },
                    )
                },
            ) {
                act = Some((17, 0.0));
            }
            if icon_btn_at(
                ui,
                Pos2::new(r - 195.0, y2),
                "vid_share",
                "create a sharing link",
                |p, c, hot, t| {
                    let _ = (hot, t);
                    icon_share(p, c, hot, t, Color32::WHITE)
                },
            ) {
                act = Some((12, 0.0));
            }
            // state-dependent actions: only what this video can actually do
            {
                let mut bx = l + 214.0;
                let mut word = |ui: &mut egui::Ui, label: &str, id: &str, code: u8, act_out: &mut Option<(u8, f64)>| {
                    let bw = (label.len() as f32 * 6.2 + 18.0).max(58.0);
                    let br = Rect::from_center_size(Pos2::new(bx + bw * 0.5, y2), Vec2::new(bw, 26.0));
                    bx += bw + 6.0;
                    let p = ui.painter();
                    p.rect_filled(br, 7.0, Color32::from_white_alpha(20));
                    p.text(
                        br.center(),
                        Align2::CENTER_CENTER,
                        label,
                        egui::FontId::proportional(11.0),
                        Color32::WHITE,
                    );
                    if ui.interact(br, egui::Id::new(id), egui::Sense::click()).clicked() {
                        *act_out = Some((code, 0.0));
                    }
                };
                word(ui, if v_archive { "unarchive" } else { "archive" }, "vid_arch", 18, &mut act);
                word(ui, if v_locked { "unlock" } else { "lock" }, "vid_lock", 19, &mut act);
                if self.viewer_confirm_del {
                    word(ui, "YES, delete", "vid_del_yes", 20, &mut act);
                } else {
                    word(ui, "delete forever", "vid_del", 21, &mut act);
                }
            }
            if icon_btn_at(
                ui,
                Pos2::new(r - 159.0, y2),
                "vid_loop",
                if looping { "stop looping" } else { "loop" },
                |p, c, hot, t| {
                    icon_loop(
                        p,
                        c,
                        hot,
                        t,
                        looping,
                        if looping { YELLOW } else { Color32::WHITE },
                    )
                },
            ) {
                act = Some((6, 0.0));
            }
        }
        if let Some((key, at, url)) = want_thumb {
            let input = cached_video(&key)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(url);
            let dims = self
                .vid
                .as_ref()
                .map(|v| (v.size.0 as u32, v.size.1 as u32))
                .unwrap_or((16, 9));
            if let Some(t) = self.seek_thumb_ask(&key, &input, at, dims) {
                self.seek_thumb = Some(t);
            }
        }
        if drop_thumb {
            self.seek_thumb = None;
        }
        if act.is_some() {
            self.ctl_touch = Instant::now();
        }
        match act {
            Some((0, _)) => self.video_toggle(),
            Some((1, t)) => self.video_seek_to(t),
            Some((2, _)) => self.video_seek_rel(-10.0),
            Some((3, _)) => self.video_seek_rel(10.0),
            Some((4, _)) => self.video_start(0.0),
            Some((5, _)) => self.video_mute(),
            Some((6, _)) => {
                if let Some(v) = self.vid.as_mut() {
                    v.looping = !v.looping;
                }
            }
            Some((7, _)) => self.toggle_fullscreen(&ui.ctx().clone()),
            Some((8, _)) => self.viewer_download(),
            Some((9, _)) => self.viewer_info_open = !self.viewer_info_open,
            Some((10, _)) => self.viewer_act("trash"),
            Some((11, _)) => self.viewer_act(if fav_here { "unfav" } else { "fav" }),
            Some((12, _)) => self.viewer_act("share"),
            Some((13, _)) => self.viewer_act("delforever"),
            Some((15, _)) => self.viewer_nav(-1),
            Some((16, _)) => self.viewer_nav(1),
            Some((17, _)) => self.viewer_act(if self.current_lib_view() == "trash" { "restore" } else { "trash" }),
            Some((18, _)) => self.viewer_act(if self.current_lib_view() == "archive" { "unarchive" } else { "archive" }),
            Some((19, _)) => self.viewer_act(if self.current_lib_view() == "locked" { "unlock" } else { "lock" }),
            Some((20, _)) => {
                self.viewer_confirm_del = false;
                self.viewer_act("delforever");
            }
            Some((21, _)) => {
                if self.confirm_del_setting {
                    self.viewer_confirm_del = true;
                } else {
                    self.viewer_act("delforever");
                }
            }
            _ => {}
        }
    }

    // one nav per swipe/scroll gesture
    fn viewer_nav_guarded(&mut self, delta: i32) {
        if self.last_nav.elapsed() < Duration::from_millis(350) {
            return;
        }
        self.last_nav = Instant::now();
        self.viewer_nav(delta);
    }

    fn viewer_nav(&mut self, delta: i32) {
        let key = match &self.viewer {
            Some(k) => k.clone(),
            None => return,
        };
        let items = self.lib_items.lock().unwrap();
        let Some(i) = items.iter().position(|x| x.media_key == key) else {
            return;
        };
        let j = (i as i64 + delta as i64).clamp(0, items.len() as i64 - 1) as usize;
        let new_key = items[j].media_key.clone();
        drop(items);
        if new_key != key {
            self.open_viewer(&new_key);
        }
    }

    // one-off library action on the photo currently open
    fn viewer_act(&mut self, action: &str) {
        let key = match &self.viewer {
            Some(k) => k.clone(),
            None => return,
        };
        if action == "copyimage" {
            let Some(it) = self
                .lib_items
                .lock()
                .unwrap()
                .iter()
                .find(|x| x.media_key == key)
                .cloned()
            else {
                return;
            };
            let tx = self.tx.clone();
            thread::spawn(move || {
                let bytes = fetch_media(&it.thumb, &library::fife_cookie_header(), "=d");
                let done = match &bytes {
                    Some(b) => copy_bytes_to_clipboard(b),
                    None => false,
                };
                tx.send(Msg::Toast {
                    msg: if done {
                        "image copied to clipboard".into()
                    } else {
                        "could not copy the image".into()
                    },
                    is_error: !done,
                })
                .ok();
            });
            return;
        }
        if action == "share" {
            let Some(s) = self.lib_session.lock().unwrap().clone() else { return };
            let tx = self.tx.clone();
            let copy_links = self.copy_links;
            let keys = vec![key.clone()];
            thread::spawn(move || {
                match library::create_share_link(&s, &keys) {
                    Some(url) => {
                        let copied = copy_links && copy_text_to_clipboard(&url);
                        tx.send(Msg::Toast {
                            msg: format!("{}{}", if copied { "link copied: " } else { "link: " }, url),
                            is_error: false,
                        })
                        .ok();
                    }
                    None => {
                        tx.send(Msg::Toast {
                            msg: "could not create a sharing link".into(),
                            is_error: true,
                        })
                        .ok();
                    }
                }
            });
            return;
        }
        let it = self
            .lib_items
            .lock()
            .unwrap()
            .iter()
            .find(|x| x.media_key == key)
            .cloned();
        let Some(it) = it else { return };
        let sess_sh = self.lib_session.clone();
        let tx = self.tx.clone();
        let items_sh = self.lib_items.clone();
        let items_gen_sh = self.items_gen.clone();
        let dedup = vec![it.dedup_key.clone()];
        // instant favorite heart feedback; the rpc lands in the background
        if action == "fav" || action == "unfav" {
            let mut items = self.lib_items.lock().unwrap();
            if let Some(i) = items.iter_mut().find(|x| x.media_key == key) {
                i.is_favorite = action == "fav";
            }
            self.items_gen.fetch_add(1, Ordering::Relaxed);
        }
        let action = action.to_string();
        // instant feedback: the RPC takes a few seconds, so acknowledge the
        // click now and let the result toast replace it when it lands
        let pending = match action.as_str() {
            "trash" => "moving to trash…",
            "restore" => "restoring…",
            "delforever" => "deleting forever…",
            "archive" => "archiving…",
            "unarchive" => "unarchiving…",
            "fav" => "adding to favourites…",
            "unfav" => "removing from favourites…",
            "lock" => "moving to locked folder…",
            "unlock" => "unlocking…",
            _ => "working…",
        };
        let _ = tx.send(Msg::Toast { msg: pending.into(), is_error: false });
        thread::spawn(move || {
            let Some(s) = sess_sh.lock().unwrap().clone() else {
                if action == "fav" || action == "unfav" {
                    let want = action == "fav";
                    for i in items_sh.lock().unwrap().iter_mut() {
                        if i.media_key == key { i.is_favorite = !want; }
                    }
                    items_gen_sh.fetch_add(1, Ordering::Relaxed);
                }
                let _ = tx.send(Msg::Toast { msg: "session lost — refresh first".into(), is_error: true });
                return;
            };
            let ok = match action.as_str() {
                "fav" => library::set_favorite(&s, &dedup, true),
                "unfav" => library::set_favorite(&s, &dedup, false),
                "archive" => library::set_archived(&s, &dedup, true),
                "unarchive" => library::set_archived(&s, &dedup, false),
                "trash" => library::set_trashed(&s, &dedup, true),
                "restore" => library::set_trashed(&s, &dedup, false),
                "rmalbum" => library::remove_from_album(&s, &vec![key.clone()]),
                "lock" => library::lock_items(&s, &dedup),
                "unlock" => library::unlock_items(&s, &dedup),
                "delforever" => {
                    library::set_trashed(&s, &dedup, true) && library::delete_forever(&s, &dedup)
                }
                _ => false,
            };
            if !ok && (action == "fav" || action == "unfav") {
                let want = action == "fav";
                for i in items_sh.lock().unwrap().iter_mut() {
                    if i.media_key == key { i.is_favorite = !want; }
                }
                items_gen_sh.fetch_add(1, Ordering::Relaxed);
            }
            let _ = tx.send(Msg::Toast {
                msg: if ok { "done".into() } else { "didn't go through — try again".into() },
                is_error: !ok,
            });
            if ok {
                let _ = tx.send(Msg::RefreshLib);
            }
        });
    }

    fn viewer_download(&self) {
        let Some(key) = self.viewer.clone() else { return };
        let it = self
            .lib_items
            .lock()
            .unwrap()
            .iter()
            .find(|x| x.media_key == key)
            .cloned();
        let Some(it) = it else { return };
        let sess_sh = self.lib_session.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::Toast { msg: "downloading original…".into(), is_error: false });
            let Some(s) = sess_sh.lock().unwrap().clone() else {
                let _ = tx.send(Msg::Toast { msg: "session lost — refresh first".into(), is_error: true });
                return;
            };
            let name = library::bulk_info(&s, &[it.media_key.clone()])
                .into_iter()
                .next()
                .map(|i| i.filename)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("{}.jpg", &it.media_key[..16.min(it.media_key.len())]))
                .replace('/', "_");
            let dir = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
            let _ = fs::create_dir_all(&dir);
            let name = if it.is_video && !name.to_lowercase().ends_with(".mp4") {
                format!("{}.mp4", name.trim_end_matches(".jpg"))
            } else {
                name
            };
            let dest = dir.join(&name);
            // a video's "original" is the stream itself, not the poster frame
            if it.is_video {
                let done = library::curl_with_cookie(
                    &[
                        format!("url = {}", library::curl_quote(&sized_url(&it.thumb, "=dv"))),
                        "location = true".into(),
                        "fail = true".into(),
                        "max-time = 900".into(),
                        format!("output = {}", library::curl_quote(&dest.to_string_lossy())),
                        "user-agent = \"Mozilla/5.0 Chrome/131.0\"".into(),
                    ],
                    &library::fife_cookie_header(),
                )
                .is_some();
                let _ = tx.send(Msg::Toast {
                    msg: if done {
                        format!("saved → {}", dest.display())
                    } else {
                        "video download failed".to_string()
                    },
                    is_error: !done,
                });
                return;
            }
            match fetch_media(&it.thumb, &library::fife_cookie_header(), "=d") {
                Some(b) => match fs::write(&dest, &b) {
                    Ok(_) => {
                        let _ = tx.send(Msg::Toast {
                            msg: format!("saved → {}", dest.display()),
                            is_error: false,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Toast { msg: format!("write failed: {e}"), is_error: true });
                    }
                },
                None => {
                    let _ = tx.send(Msg::Toast { msg: "download failed".into(), is_error: true });
                }
            }
        });
    }

    fn viewer_ui(&mut self, ctx: &egui::Context) {
        use egui::Key;
        // keyboard: in a video arrows seek, otherwise they flip photos
        let vid_key = self
            .vid
            .as_ref()
            .map(|v| Some(v.key.clone()) == self.viewer)
            .unwrap_or(false);
        let mut want_fullscreen = false;
        // These must be read OUTSIDE ctx.input(): wants_keyboard_input() and
        // memory_mut() both re-enter the same egui lock. Nesting them inside
        // ctx.input(|i| ...) deadlocked the UI thread on a competing writer,
        // making the whole app ignore every click.
        let wants_kb = ctx.wants_keyboard_input();
        let esc = ctx.input(|i| i.key_pressed(Key::Escape));
        if wants_kb && esc {
            ctx.memory_mut(|m| m.request_focus(egui::Id::NULL));
        } else if !wants_kb {
        ctx.input(|i| {
            if i.key_pressed(Key::Escape) && !self.show_keys {
                self.close_viewer();
            }
            if !vid_key && i.key_pressed(Key::Num0) {
                self.viewer_zoom = 1.0;
            }
            if vid_key {
                // 10s steps left/right, a minute up/down, space to pause
                if i.key_pressed(Key::ArrowRight) {
                    self.video_seek_rel(self.seek_step);
                }
                if i.key_pressed(Key::ArrowLeft) {
                    self.video_seek_rel(-self.seek_step);
                }
                if i.key_pressed(Key::ArrowUp) {
                    self.video_seek_rel(60.0);
                }
                if i.key_pressed(Key::ArrowDown) {
                    self.video_seek_rel(-60.0);
                }
                if i.key_pressed(Key::Space) || i.key_pressed(Key::K) {
                    self.video_toggle();
                }
                if i.key_pressed(Key::J) {
                    self.video_seek_rel(-self.seek_step);
                }
                if i.key_pressed(Key::L) {
                    self.video_seek_rel(self.seek_step);
                }
                if i.key_pressed(Key::M) {
                    self.video_mute();
                }
                for (n, k) in [
                    (0, Key::Num0),
                    (1, Key::Num1),
                    (2, Key::Num2),
                    (3, Key::Num3),
                    (4, Key::Num4),
                    (5, Key::Num5),
                    (6, Key::Num6),
                    (7, Key::Num7),
                    (8, Key::Num8),
                    (9, Key::Num9),
                ] {
                    if i.key_pressed(k) {
                        let dur = self.vid.as_ref().map(|v| v.dur).unwrap_or(0.0);
                        self.video_seek_to(dur * n as f64 / 10.0);
                    }
                }
            } else {
                if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::PageDown) {
                    self.viewer_nav(1);
                }
                if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::PageUp) {
                    self.viewer_nav(-1);
                }
                if i.key_pressed(Key::ArrowDown) {
                    self.viewer_nav(1);
                }
                if i.key_pressed(Key::ArrowUp) {
                    self.viewer_nav(-1);
                }
                if i.key_pressed(Key::Home) {
                    self.viewer_nav(-100000);
                }
                if i.key_pressed(Key::End) {
                    self.viewer_nav(100000);
                }
            }
            if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) {
                self.viewer_zoom = (self.viewer_zoom * 1.25).min(8.0);
            }
            if i.key_pressed(Key::Minus) {
                self.viewer_zoom = (self.viewer_zoom / 1.25).max(0.5);
            }
            if i.key_pressed(Key::I) {
                self.viewer_info_open = !self.viewer_info_open;
            }
            if i.key_pressed(Key::S) {
                // s pauses/resumes a slideshow without changing the saved
                // interval — the Settings value stays intact
                self.slideshow_secs = if self.slideshow_secs > 0.0 { 0.0 } else { 3.0 };
                self.slideshow_at = Instant::now();
            }
            if i.key_pressed(Key::F) {
                want_fullscreen = true;
            }
            // horizontal two-finger scroll at fit zoom = prev/next photo
            if self.viewer_zoom <= 1.0 && i.raw_scroll_delta.x.abs() > 8.0 {
                let d = if i.raw_scroll_delta.x > 0.0 { 1 } else { -1 };
                self.viewer_nav_guarded(d);
            }
        });
        } // end else-if !wants_kb
        if want_fullscreen {
            self.toggle_fullscreen(ctx);
        }
        self.video_tick();
        // VLC's default: chrome fades 2 s after the pointer stops. Any movement
        // wakes it, and so does starting or pausing the video (set elsewhere).
        let ptr = ctx.input(|i| i.pointer.hover_pos());
        if let Some(p) = ptr {
            if self.last_pointer.map(|q| (p - q).length() > 1.0).unwrap_or(true) {
                self.ctl_move = Instant::now();
                self.last_pointer = Some(p);
            }
        } else {
            self.last_pointer = None;
        }
        let chrome_awake = self.ctl_move.elapsed() < Duration::from_secs(2)
            || std::env::var("GPHOTOS_HOVERBAR").is_ok();
        self.seek_thumb_tick(ctx);
        let key = match &self.viewer {
            Some(k) => k.clone(),
            None => return,
        };
        let cv = self.current_lib_view();
        let in_trash = cv == "trash";
        let in_archive = cv == "archive";
        let in_locked = cv == "locked";
        let in_shared = cv.starts_with("link:");
        let in_album = cv.starts_with("album:");
        let item = self
            .lib_items
            .lock()
            .unwrap()
            .iter()
            .find(|x| x.media_key == key)
            .cloned();
        let Some(item) = item else {
            // the photo left the view (trashed/restored/refreshed) — close
            self.close_viewer();
            return;
        };
        let img = self
            .viewer_img
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(k, _)| k == &key)
            .map(|(_, i)| (i.width(), i.height(), i.clone()));
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("viewer_overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.left_top())
            .show(ctx, |ui| {
                ui.allocate_ui_at_rect(screen, |ui| {
                    ui.set_clip_rect(screen);
                    ui.painter()
                        .rect_filled(screen, 0.0, Color32::from_rgb(0x1c, 0x18, 0x14));
                    let vid_here = self
                        .vid
                        .as_ref()
                        .map(|v| v.key == key)
                        .unwrap_or(false);
                    let top = Rect::from_min_size(screen.left_top(), Vec2::new(screen.width(), 36.0));
                    // a video keeps its own transport bar, so the photo action
                    // strip is not drawn at all: that is a fifth of the window
                    // handed back to the picture
                    let bottom_h = if vid_here { 0.0 } else { 62.0 };
                    let bottom = Rect::from_min_size(
                        Pos2::new(screen.left(), screen.bottom() - bottom_h),
                        Vec2::new(screen.width(), bottom_h),
                    );
                    // the info panel takes its own 300 px, so the picture and
                    // the controls never slide underneath it
                    let info_w = if self.viewer_info_open { 300.0 } else { 0.0 };
                    let mid = Rect::from_min_max(
                        Pos2::new(screen.left(), top.bottom()),
                        Pos2::new((screen.right() - info_w).max(screen.left() + 200.0), bottom.top()),
                    );
                    // ---- top bar: filename left, round close top-right
                    let info_title = self
                        .viewer_info
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|i| i.filename.clone())
                        .unwrap_or_default();
                    let shown = if !item.title.is_empty() {
                        item.title.clone()
                    } else {
                        info_title
                    };
                    let shown = if shown.is_empty() {
                        "loading info…".to_string()
                    } else {
                        shown
                    };
                    let is_vid = item.is_video || is_video_file(&shown);
                    ui.allocate_ui_at_rect(top, |ui| {
                        ui.set_clip_rect(top);
                        if is_vid && !chrome_awake {
                            return;
                        }
                        ui.horizontal_centered(|ui| {
                            ui.add_space(14.0);
                            ui.label(
                                egui::RichText::new(truncate(&shown, 72))
                                    .size(13.5)
                                    .strong()
                                    .color(Color32::WHITE),
                            );
                            // exactly one place says what the player is doing:
                            // seeking, reading the saved copy, or streaming
                            if is_vid {
                                let seeking = self
                                    .vid
                                    .as_ref()
                                    .map(|v| v.key == key && v.seeking)
                                    .unwrap_or(false);
                                let (word, col) = if seeking {
                                    (STATE_SEEKING, YELLOW)
                                } else if cached_video(&item.media_key).is_some() {
                                    (STATE_ON_DISK, Color32::from_white_alpha(145))
                                } else {
                                    (STATE_STREAMING, YELLOW)
                                };
                                ui.label(egui::RichText::new(word).size(11.0).color(col));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.add_space(6.0);
                                    if icon_btn(ui, "v_close", "close (esc)", |p, c, hot, t| {
                                        icon_close(p, c, hot, t, Color32::from_white_alpha(190))
                                    }) {
                                        self.close_viewer();
                                    }
                                },
                            );
                        });
                    });
                    // ---- center: the photo, fit at zoom 1, scroll to pan when zoomed
                    ui.allocate_ui_at_rect(mid, |ui| {
                        ui.set_clip_rect(mid);
                        if vid_here {
                            // VLC-style: the controls get their own strip, never
                            // drawn on top of the picture
                            // uosc's rule: controls live near the pointer, not
                            // everywhere. within 40 px they are fully out, past
                            // 120 px they fold into a two-pixel progress line, and
                            // the picture takes the space back instead of leaving it
                            // empty. Paused or just after a key they stay out.
                            let paused_here = self
                                .vid
                                .as_ref()
                                .map(|v| !v.playing || v.ended)
                                .unwrap_or(true);
                            let show_ctl = paused_here || chrome_awake;
                            let ctl_h = if show_ctl {
                                64.0f32.min(mid.height() * 0.4)
                            } else {
                                4.0
                            };
                            let ctl = Rect::from_min_size(
                                Pos2::new(mid.left(), mid.bottom() - ctl_h),
                                Vec2::new(mid.width(), ctl_h),
                            );
                            let pic = Rect::from_min_max(
                                mid.min,
                                Pos2::new(mid.right(), ctl.top()),
                            );
                            self.video_center_ui(ui, pic, ctl);
                        } else if let Some((tw, th, ref img)) = img {
                            let tex_id = self
                                .tex
                                .entry(format!("full:{}", key))
                                .or_insert_with(|| {
                                    ui.ctx().load_texture(
                                        format!("full:{}", key),
                                        img.clone(),
                                        egui::TextureOptions::LINEAR,
                                    )
                                })
                                .id();
                            let (tw, th) = (tw as f32, th as f32);
                            let fit = (mid.width() / tw).min(mid.height() / th) * self.viewer_zoom;
                            let disp = Vec2::new(tw * fit, th * fit);
                            if disp.x <= mid.width() && disp.y <= mid.height() {
                                // fits the screen: drag left/right flips to the
                                // next/previous photo, like the phone app
                                let (ar, aresp) = ui.allocate_exact_size(
                                    Vec2::new(mid.width(), mid.height()),
                                    egui::Sense::click_and_drag(),
                                );
                                let pos = ar.center() - disp * 0.5;
                                let shown = Rect::from_min_size(pos, disp);
                                if aresp.clicked() {
                                    if let Some(p) = aresp.interact_pointer_pos() {
                                        if p.x < shown.left() {
                                            self.viewer_nav(-1);
                                        } else if p.x > shown.right() {
                                            self.viewer_nav(1);
                                        }
                                    }
                                }
                                ui.painter().image(
                                    tex_id,
                                    Rect::from_min_size(pos, disp),
                                    Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                                // videos: big play button over the poster frame
                                if is_vid {
                                    let c = ar.center();
                                    let hot = aresp
                                        .hover_pos()
                                        .map(|p| p.distance(c) < 36.0)
                                        .unwrap_or(false);
                                    let r = if hot { 27.0 } else { 24.0 };
                                    ui.painter().circle_filled(
                                        c,
                                        r,
                                        if hot {
                                            Color32::from_white_alpha(235)
                                        } else {
                                            Color32::from_white_alpha(185)
                                        },
                                    );
                                    ui.painter().add(egui::Shape::convex_polygon(
                                        vec![
                                            c + Vec2::new(-7.0, -11.0),
                                            c + Vec2::new(12.0, 0.0),
                                            c + Vec2::new(-7.0, 11.0),
                                        ],
                                        INK,
                                        Stroke::NONE,
                                    ));
                                    if hot {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                    }
                                    if aresp.clicked() {
                                        if let Some(p) = aresp.interact_pointer_pos() {
                                            if p.distance(c) < 36.0 {
                                                self.open_video(&item);
                                            }
                                        }
                                    }
                                }
                                let d = aresp.drag_delta();
                                if d.x.abs() > 50.0 && d.x.abs() > d.y.abs() {
                                    self.viewer_nav_guarded(if d.x < 0.0 { 1 } else { -1 });
                                }
                            } else {
                                // zoomed in: scroll pans both ways
                                let content =
                                    Vec2::new(disp.x.max(mid.width()), disp.y.max(mid.height()));
                                egui::ScrollArea::both()
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        let (crect, _) =
                                            ui.allocate_exact_size(content, egui::Sense::hover());
                                        let pos = crect.center() - disp * 0.5;
                                        ui.painter().image(
                                            tex_id,
                                            Rect::from_min_size(pos, disp),
                                            Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0, 1.0)),
                                            Color32::WHITE,
                                        );
                                    });
                            }
                        } else {
                            ui.centered_and_justified(|ui| {
                                ui.label(egui::RichText::new("loading full size…").color(INK_SOFT));
                            });
                        }
                    });
                    // ---- info side panel
                    if self.viewer_info_open {
                        let info_rect = Rect::from_min_max(
                            Pos2::new((screen.right() - 300.0).max(screen.left()), top.bottom()),
                            Pos2::new(screen.right(), bottom.top()),
                        );
                        ui.allocate_ui_at_rect(info_rect, |ui| {
                            ui.set_clip_rect(info_rect);
                            ui.painter().rect_filled(info_rect, 0.0, Color32::from_black_alpha(210));
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                ui.add_space(10.0);
                                ui.heading(egui::RichText::new("info").color(Color32::WHITE));
                                let info = self.viewer_info.lock().unwrap().clone();
                                if let Some(inf) = &info {
                                    if !inf.filename.is_empty() {
                                        ui.label(egui::RichText::new(format!("file: {}", inf.filename)).color(Color32::WHITE));
                                    }
                                    if inf.size > 0 {
                                        ui.label(egui::RichText::new(format!("size: {}", human_size(inf.size))).color(Color32::WHITE));
                                    }
                                    if let Some(q) = inf.original_quality {
                                        ui.label(egui::RichText::new(format!("quality: {}", if q { "original" } else { "storage saver" })).color(Color32::WHITE));
                                    }
                                }
                                if item.w > 0 {
                                    ui.label(egui::RichText::new(format!("pixels: {} × {}", item.w, item.h)).color(Color32::WHITE));
                                }
                                // camera / place / owner / source, from the item's
                                // own detail record (fDcn4b)
                                let ext = self.viewer_ext.lock().unwrap().clone();
                                if let Some(e) = &ext {
                                    if !e.camera.is_empty() && e.camera != "0" {
                                        ui.label(egui::RichText::new(format!("camera: {}", e.camera)).color(Color32::WHITE));
                                    }
                                    if !e.place.is_empty() {
                                        ui.label(egui::RichText::new(format!("place: {}", e.place)).color(Color32::WHITE));
                                    }
                                    if let Some((lat, lon)) = e.coords {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(format!("map: {:.4}, {:.4}", lat, lon))
                                                    .color(Color32::WHITE),
                                            );
                                            if ui.small_button("open").clicked() {
                                                open_url(&format!(
                                                    "https://www.google.com/maps/search/?api=1&query={},{}",
                                                    lat, lon
                                                ));
                                            }
                                        });
                                    }
                                    if !e.owner.is_empty() {
                                        ui.label(egui::RichText::new(format!("owner: {}", e.owner)).color(Color32::WHITE));
                                    }
                                    if !e.source.is_empty() {
                                        ui.label(egui::RichText::new(format!("uploaded from: {}", e.source)).color(Color32::WHITE));
                                    }
                                    if !e.albums.is_empty() {
                                        ui.label(egui::RichText::new(format!("in albums: {}", e.albums.join(", "))).color(Color32::WHITE));
                                    }
                                }
                                let dl = date_label(item.ts);
                                if !dl.is_empty() {
                                    ui.label(egui::RichText::new(format!("date: {}", dl)).color(Color32::WHITE));
                                }
                                // manual date correction (same DaSgWe path as
                                // "date from name", but with a typed date)
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.viewer_date)
                                            .desired_width(96.0)
                                            .hint_text("YYYY-MM-DD"),
                                    );
                                    if ui.button("set date").clicked() {
                                        let typed = self.viewer_date.trim().to_string();
                                        if library::parse_ymd(&typed).is_some() {
                                            let dedup = item.dedup_key.clone();
                                            let sess = self.lib_session.clone();
                                            let key2 = item.media_key.clone();
                                            let tx = self.tx.clone();
                                            thread::spawn(move || {
                                                let Some(s) = sess.lock().unwrap().clone() else { return };
                                                let n =
                                                    library::set_timestamp_manual(&s, &[(key2, dedup, typed)]);
                                                let _ = tx.send(Msg::Toast {
                                                    msg: if n > 0 { "date updated".into() } else { "date update failed".into() },
                                                    is_error: n == 0,
                                                });
                                                if n > 0 {
                                                    let _ = tx.send(Msg::RefreshLib);
                                                }
                                            });
                                            self.viewer_date.clear();
                                        }
                                    }
                                });
                                ui.add_space(6.0);
                                ui.label(egui::RichText::new("caption").color(Color32::WHITE));
                                let loaded = info
                                    .as_ref()
                                    .map(|i| i.description.clone())
                                    .unwrap_or_default();
                                // seed once per photo: refilling whenever the
                                // field is empty makes clearing a caption pop
                                // the old text straight back
                                if self.caption_loaded_for.as_deref() != Some(key.as_str()) {
                                    self.viewer_desc = loaded;
                                    self.caption_loaded_for = Some(key.to_string());
                                }
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.viewer_desc)
                                        .desired_width(250.0)
                                        .hint_text("add a caption…"),
                                );
                                if ui.button("save caption").clicked() {
                                    self.save_caption(&item, self.viewer_desc.clone());
                                }
                            });
                        });
                    }
                    // ---- bottom controls: two tidy rows, nothing clipped
                    // ---- bottom: one action row for photos, full width. A
                    // video skips it entirely, so its picture is taller.
                    if bottom_h > 0.0 {
                        ui.allocate_ui_at_rect(bottom, |ui| {
                            ui.set_clip_rect(bottom);
                            ui.painter()
                                .rect_filled(bottom, 0.0, Color32::from_rgb(0x16, 0x13, 0x10));
                            ui.horizontal_centered(|ui| {
                                ui.add_space(12.0);
                                if icon_btn(ui, "v_prev", "previous (left arrow)", |p, c, hot, _t| {
                                    icon_chevron(p, c, hot, -1.0, Color32::WHITE)
                                }) {
                                    self.viewer_nav(-1);
                                }
                                if icon_btn(ui, "v_next", "next (right arrow)", |p, c, hot, _t| {
                                    icon_chevron(p, c, hot, 1.0, Color32::WHITE)
                                }) {
                                    self.viewer_nav(1);
                                }
                                ui.add_space(4.0);
                                let fav = item.is_favorite;
                                if icon_btn(
                                    ui,
                                    "v_fav",
                                    if fav { "remove favourite" } else { "favourite" },
                                    |p, c, hot, t| {
                                        icon_star(
                                            p,
                                            c,
                                            hot,
                                            t,
                                            fav,
                                            if fav { RED } else { Color32::WHITE },
                                        )
                                    },
                                ) {
                                    self.viewer_act(if fav { "unfav" } else { "fav" });
                                }
                                if ui.button("share").clicked() {
                                    self.viewer_act("share");
                                }
                                if icon_btn(
                                    ui,
                                    "v_archive",
                                    if in_archive { "unarchive" } else { "archive" },
                                    |p, c, hot, t| {
                                        icon_archive(
                                            p,
                                            c,
                                            hot,
                                            t,
                                            if in_archive { YELLOW } else { Color32::WHITE },
                                        )
                                    },
                                ) {
                                    self.viewer_act(if in_archive { "unarchive" } else { "archive" });
                                }
                                if icon_btn(
                                    ui,
                                    "v_trash",
                                    if in_trash { "restore" } else { "move to trash" },
                                    |p, c, hot, t| {
                                        icon_trash(
                                            p,
                                            c,
                                            hot,
                                            t,
                                            if in_trash { YELLOW } else { Color32::WHITE },
                                        )
                                    },
                                ) {
                                    self.viewer_act(if in_trash { "restore" } else { "trash" });
                                }
                                if icon_btn(ui, "v_save", "save a copy", |p, c, hot, t| {
                                    icon_download(p, c, hot, t, Color32::WHITE)
                                }) {
                                    self.viewer_download();
                                }
                                if ui.button("copy").clicked() {
                                    self.viewer_act("copyimage");
                                }
                                if icon_btn(
                                    ui,
                                    "v_info",
                                    if self.viewer_info_open { "hide details" } else { "details" },
                                    |p, c, hot, t| {
                                        icon_info(
                                            p,
                                            c,
                                            hot,
                                            t,
                                            if self.viewer_info_open { YELLOW } else { Color32::WHITE },
                                        )
                                    },
                                ) {
                                    self.viewer_info_open = !self.viewer_info_open;
                                }
                                ui.add_space(6.0);
                                // permanent delete stays available everywhere,
                                // behind the same two-step confirmation
                                if !self.viewer_confirm_del {
                                    if ui
                                        .button(egui::RichText::new("delete forever").color(RED))
                                        .clicked()
                                    {
                                        if self.confirm_del_setting {
                                            self.viewer_confirm_del = true;
                                        } else {
                                            self.viewer_act("delforever");
                                        }
                                    }
                                } else {
                                    ui.label(
                                        egui::RichText::new("permanent, no undo")
                                            .color(RED)
                                            .strong()
                                            .size(11.0),
                                    );
                                    if ui
                                        .button(egui::RichText::new("yes, delete").color(RED))
                                        .clicked()
                                    {
                                        self.viewer_confirm_del = false;
                                        self.viewer_act("delforever");
                                    }
                                    if ui.button("cancel").clicked() {
                                        self.viewer_confirm_del = false;
                                    }
                                }
                                // only the actions that make sense for this
                                // item are offered; nothing that would be a no-op
                                if in_locked {
                                    if ui.button("unlock").clicked() {
                                        self.viewer_act("unlock");
                                    }
                                } else if !in_trash {
                                    if ui.button("lock").clicked() {
                                        self.viewer_act("lock");
                                    }
                                }
                                if in_album && ui.button("remove from album").clicked() {
                                    self.viewer_act("rmalbum");
                                }
                                if in_shared {
                                    if ui.button("new link").clicked() {
                                        self.viewer_act("share");
                                    }
                                    if ui.button("manage").clicked() {
                                        open_url(&format!("https://photos.google.com/links/{}", &cv[5..]));
                                    }
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.add_space(10.0);
                                        if icon_btn(ui, "v_full", "fullscreen (f)", |p, c, hot, t| {
                                            icon_fullscreen(p, c, hot, t, Color32::WHITE)
                                        }) {
                                            self.toggle_fullscreen(&ui.ctx().clone());
                                        }
                                        if ui.button("fit").clicked() {
                                            self.viewer_zoom = 1.0;
                                        }
                                        if ui.button("+").clicked() {
                                            self.viewer_zoom = (self.viewer_zoom * 1.25).min(8.0);
                                        }
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{:.0}%",
                                                self.viewer_zoom * 100.0
                                            ))
                                            .size(11.0)
                                            .color(Color32::from_white_alpha(150)),
                                        );
                                        if ui.button("-").clicked() {
                                            self.viewer_zoom =
                                                (self.viewer_zoom / 1.25).max(0.5);
                                        }
                                        if ui.button("open on web").clicked() {
                                            open_url(&format!(
                                                "https://photos.google.com/photo/{}",
                                                key
                                            ));
                                        }
                                    },
                                );
                            });
                        });
                    }
                });
            });
    }

    fn save_caption(&mut self, item: &library::MediaItem, desc: String) {
        let sess_sh = self.lib_session.clone();
        let tx = self.tx.clone();
        let dedup = item.dedup_key.clone();
        thread::spawn(move || {
            let Some(s) = sess_sh.lock().unwrap().clone() else { return };
            let ok = library::set_item_description(&s, &dedup, &desc);
            let _ = tx.send(Msg::Toast {
                msg: if ok { "caption saved".into() } else { "caption didn't save — try again".into() },
                is_error: !ok,
            });
        });
    }

    // ---- account switcher ------------------------------------------------------

    fn open_switch(&mut self) {
        self.switch_open = true;
        // show the saved accounts immediately (so log-out/switch buttons work
        // even when Google's discovery endpoint is unavailable), then replace
        // the list with live discovery when it lands
        let mut known: Vec<library::AccountRecord> = library::load_registry()
            .into_iter()
            .filter(|r| !r.hidden)
            .collect();
        // a legacy-selected account may have no registry row — include the
        // current identity so "log out" is always reachable for it
        if let Some((jar, email)) = self.selected.clone() {
            if !known.iter().any(|r| r.jar == jar && r.email == email) {
                known.push(library::AccountRecord {
                    jar,
                    email,
                    name: String::new(),
                    hidden: false,
                    last_seen: 0,
                    last_ok: 0,
                });
            }
        }
        if !known.is_empty() {
            self.switch_list.lock().unwrap().clone_from(&known);
        }
        let tx = self.tx.clone();
        let epoch_sh = self.account_epoch.clone();
        let epoch = epoch_sh.load(Ordering::Relaxed);
        thread::spawn(move || {
            let rows = library::discover_accounts();
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                return;
            }
            let _ = tx.send(Msg::Accounts(rows));
        });
    }

    // Hidden is a jar+email file, mirrored into the registry so a rescan cannot
    // resurrect the owner's choice. Two profiles with one email stay independent.
    fn set_hidden(&mut self, jar: &str, email: &str, hidden: bool) {
        let key = (jar.to_string(), email.to_string());
        if hidden {
            self.hidden.insert(key);
        } else {
            self.hidden.remove(&key);
        }
        write_hidden(&self.hidden);
        let mut rows = library::load_registry();
        for r in &mut rows {
            if r.jar == jar && r.email == email {
                r.hidden = hidden;
            }
        }
        library::save_registry(&rows);
        if hidden {
            self.pending_adds.retain(|p| p.jar != jar || p.email != email);
        }
    }

    // Removing a staged identity only drops it from the list; removing a known
    // one also forgets it in the registry. Never adds a new row.
    fn remove_account(&mut self, jar: &str, email: &str) {
        let rows: Vec<_> = library::load_registry()
            .into_iter()
            .filter(|r| r.jar != jar || r.email != email)
            .collect();
        library::save_registry(&rows);
        self.switch_list
            .lock()
            .unwrap()
            .retain(|r| r.jar != jar || r.email != email);
        self.hidden.remove(&(jar.to_string(), email.to_string()));
        write_hidden(&self.hidden);
        self.pending_adds.retain(|p| p.jar != jar || p.email != email);
    }

    // Log out one account: forget its cached session and, if it is active,
    // leave the signed-in page. Browser profile cookies are deliberately left
    // alone — the user logs out of Google in the browser themselves.
    fn logout_one(&mut self, jar: &str, email: &str) {
        library::forget_session(jar, email);
        if self.selected.as_ref() == Some(&(jar.to_string(), email.to_string())) {
            self.finish_logout();
        }
    }

    fn logout_all(&mut self) {
        library::forget_all_sessions();
        self.finish_logout();
    }

    fn finish_logout(&mut self) {
        set_setting("logged_out", "1");
        set_setting("selected_jar", "");
        set_setting("selected_email", "");
        self.selected = None;
        *self.lib_session.lock().unwrap() = None;
        self.account_epoch.fetch_add(1, Ordering::Relaxed);
        self.reset_for_account();
        *self.page.lock().unwrap() = "signin".to_string();
        self.switch_open = false;
        self.pending_adds.clear();
        self.add_poll = None;
    }

    fn switch_ui(&mut self, ctx: &egui::Context) {
        let list = self.switch_list.lock().unwrap().clone();
        let cur = self.selected.clone();
        let registry = library::load_registry();
        let mut jars: Vec<String> = list.iter().map(|r| r.jar.clone()).collect();
        jars.sort();
        jars.dedup();
        if self.add_jar.is_empty() {
            self.add_jar = cur.as_ref().map(|(j, _)| j.clone()).unwrap_or_default();
        }
        if !jars.is_empty() && !jars.contains(&self.add_jar) {
            self.add_jar = jars[0].clone();
        }
        egui::Window::new("accounts")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("accounts in this machine's browser:");
                let mut switch_to: Option<AccountKey> = None;
                let mut hide: Option<AccountKey> = None;
                let mut remove: Option<AccountKey> = None;
                let mut add: Option<usize> = None;
                let mut ignore: Option<usize> = None;
                let mut restore: Option<AccountKey> = None;
                let mut logout_one: Option<AccountKey> = None;
                let mut logout_all = false;
                let mut shown = 0;
                for a in &list {
                    let key = (a.jar.clone(), a.email.clone());
                    if a.email.is_empty() || is_hidden(&self.hidden, &a.jar, &a.email) {
                        continue;
                    }
                    shown += 1;
                    let in_use = cur.as_ref() == Some(&key);
                    let is_new = !registry.iter().any(|r| same_account(r, a));
                    ui.horizontal(|ui| {
                        if in_use {
                            ui.colored_label(GREEN, "in use");
                        } else if ui.button("use").clicked() {
                            switch_to = Some(key.clone());
                        }
                        ui.label(&a.email);
                        ui.label(
                            egui::RichText::new(truncate(&a.jar, 16))
                                .size(10.0)
                                .color(INK_SOFT),
                        );
                        if is_new {
                            ui.label(egui::RichText::new("new").size(10.0).color(BLUE));
                        }
                        if ui.button(egui::RichText::new("hide").color(INK_SOFT)).clicked() {
                            hide = Some(key.clone());
                        }
                        if ui.button(egui::RichText::new("remove").color(RED)).clicked() {
                            remove = Some(key.clone());
                        }
                        if ui.button(egui::RichText::new("log out").color(RED)).clicked() {
                            logout_one = Some(key.clone());
                        }
                    });
                }
                // explicit confirmation is the only path that writes a new row
                for (i, p) in self.pending_adds.iter().enumerate() {
                    if is_hidden(&self.hidden, &p.jar, &p.email) {
                        continue;
                    }
                    let label = format!("Add {}?", truncate(&p.email, 28));
                    ui.horizontal(|ui| {
                        ui.colored_label(BLUE, label);
                        if ui.button("Add").clicked() {
                            add = Some(i);
                        }
                        if ui.button("Ignore").clicked() {
                            ignore = Some(i);
                        }
                    });
                }
                if let Some(sel) = switch_to {
                    self.switch_open = false;
                    self.do_switch(sel);
                }
                if let Some(i) = add {
                    if let Some(rec) = self.pending_adds.get(i).cloned() {
                        let key = (rec.jar.clone(), rec.email.clone());
                        let mut rows = library::load_registry();
                        rows = confirm_account(&rows, &rec);
                        library::save_registry(&rows);
                        self.pending_adds.remove(i);
                        self.add_poll = None;
                        self.switch_open = false;
                        self.do_switch(key);
                    }
                }
                if let Some(i) = ignore {
                    if let Some(rec) = self.pending_adds.get(i).cloned() {
                        self.pending_adds.remove(i);
                        self.set_hidden(&rec.jar, &rec.email, true);
                    }
                }
                if let Some((jar, email)) = hide {
                    // never hide the identity currently in use
                    if cur.as_ref() != Some(&(jar.clone(), email.clone())) {
                        self.set_hidden(&jar, &email, true);
                    }
                }
                if let Some((jar, email)) = remove {
                    if cur.as_ref() != Some(&(jar.clone(), email.clone())) {
                        self.remove_account(&jar, &email);
                    }
                }
                if let Some((jar, email)) = logout_one {
                    self.logout_one(&jar, &email);
                }
                if logout_all {
                    self.logout_all();
                }
                // hidden accounts must be recoverable, otherwise a user can
                // hide every account and lock themselves out of switching
                let hidden_now: Vec<AccountKey> = self.hidden.iter().cloned().collect();
                if !hidden_now.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("hidden:").size(11.0).color(INK_SOFT));
                    for (j, em) in &hidden_now {
                        let label = format!("restore {} ({})", truncate(em, 18), truncate(j, 10));
                        if ui.small_button(label).clicked() {
                            restore = Some((j.clone(), em.clone()));
                        }
                    }
                    if let Some((j, em)) = restore {
                        self.set_hidden(&j, &em, false);
                    }
                }
                if shown == 0 {
                    ui.label("none found yet — checking…");
                }
                ui.separator();
                if ui.button(egui::RichText::new("log out of ALL accounts").color(RED)).clicked() {
                    logout_all = true;
                }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("add to:").size(11.0).color(INK_SOFT));
                    if jars.is_empty() {
                        ui.label("(no profile found yet — rescan)");
                    } else {
                        egui::ComboBox::from_id_source("add jar")
                            .selected_text(truncate(&self.add_jar, 18))
                            .show_ui(ui, |ui| {
                                for j in &jars {
                                    ui.selectable_value(&mut self.add_jar, j.clone(), truncate(j, 22));
                                }
                            });
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("+ add account").clicked() {
                        self.add_account(false);
                    }
                    if ui.button("+ create new account").clicked() {
                        self.add_account(true);
                    }
                    if ui.button("rescan").clicked() {
                        self.open_switch();
                    }
                    if self.add_poll.is_some() && ui.button("cancel").clicked() {
                        if let Some(p) = &self.add_poll {
                            p.cancel.store(true, Ordering::Relaxed);
                        }
                        self.add_poll = None;
                    }
                });
                // Windows ABE: import cookies a local extension exported. Never
                // show or log the file's contents — only a count on success.
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("import cookies file:")
                            .size(11.0)
                            .color(INK_SOFT),
                    );
                    ui.text_edit_singleline(&mut self.cookie_import_path);
                    if ui.button("import").clicked() {
                        let path = self.cookie_import_path.clone();
                        let (msg, is_error) = if path.trim().is_empty() {
                            ("enter the path to a local cookie export first".to_string(), true)
                        } else {
                            match import_cookie_file(std::path::Path::new(&path)) {
                                Ok(n) => (format!("imported {n} account(s) — rescan to pick them up"), false),
                                Err(()) => ("cookie export rejected: file missing or invalid format".into(), true),
                            }
                        };
                        self.pending_toasts.push((msg, is_error, Instant::now()));
                    }
                });
                if let Some(p) = &self.add_poll {
                    ui.label(
                        egui::RichText::new(format!(
                            "watching {} for a new sign-in (every 3s, up to 3 min)…",
                            truncate(&p.jar, 24)
                        ))
                        .size(11.0)
                        .color(BLUE),
                    );
                }
                ui.label(
                    egui::RichText::new(
                        "add/create opens the chosen profile's Google sign-in and detects the new login automatically. Only Add saves it; use just selects. hide keeps an identity out of this list; remove forgets it.",
                    )
                    .size(11.0)
                    .color(INK_SOFT),
                );
                if ui.button("close").clicked() {
                    self.switch_open = false;
                }
            });
    }

    // every account starts from the same clean state: otherwise the second
    // account inherits the first one's timeline, sizes and measurement
    fn reset_for_account(&mut self) {
        self.lib_busy.store(false, Ordering::Relaxed);
        self.scan_started.store(false, Ordering::Relaxed);
        self.hog_stats = None;
        *self.all_items.lock().unwrap() = vec![];
        self.sizes.lock().unwrap().clear();
        *self.charged.lock().unwrap() = Default::default();
        *self.lib_items.lock().unwrap() = vec![];
        self.items_gen.fetch_add(1, Ordering::Relaxed);
        *self.lib_next.lock().unwrap() = None;
        *self.lib_albums.lock().unwrap() = vec![];
        *self.lib_links.lock().unwrap() = vec![];
        *self.lib_selected.lock().unwrap() = Default::default();
        self.album_selected.clear();
        self.sel_anchor = None;
        self.month_cache.clear();
        self.month_cache_key = usize::MAX;
        self.date_labels.clear();
        self.view_prev.lock().unwrap().clear();
        self.grid_frac = 0.0;
        self.grid_want = None;
        self.tex.clear();
        self.thumbs.lock().unwrap().clear();
        self.thumb_order.clear();
        self.sorted_cache.clear();
        self.sorted_cache_key = (u64::MAX, u8::MAX);
        self.view_snapshots.lock().unwrap().clear();
        self.pending_toasts.clear();
        self.undo = None;
        self.vid_urls.lock().unwrap().clear();
        self.pending_seek = None;
        self.scroll_anchor = None;
        self.video_stop();
        self.resume.clear();
        self.vid = None;
        self.viewer = None;
    }

    fn do_switch(&mut self, sel: (String, String)) {
        set_setting("logged_out", "0");
        // bump first: in-flight workers from the previous identity must not
        // inject their data into the newly selected account
        self.account_epoch.fetch_add(1, Ordering::Relaxed);
        self.selected = Some(sel.clone());
        set_setting("selected_jar", &sel.0);
        set_setting("selected_email", &sel.1);
        self.reset_for_account();
        // invalidate the OLD account's session immediately: a worker starting
        // before the replacement capture lands must see no session, not a valid
        // one it would happily RPC with under the new epoch
        *self.lib_session.lock().unwrap() = None;
        let sess_sh = self.lib_session.clone();
        let tx = self.tx.clone();
        let epoch_sh = self.account_epoch.clone();
        let epoch = epoch_sh.load(Ordering::Relaxed);
        thread::spawn(move || {
            let s = library::Session::capture_identity(&sel.0, &sel.1);
            if epoch_sh.load(Ordering::Relaxed) != epoch {
                return;
            }
            let ok = s.as_ref().map(|s| !s.email.is_empty()).unwrap_or(false);
            *sess_sh.lock().unwrap() = s;
            let _ = tx.send(Msg::SessionReady(ok));
        });
    }

    // toasts bubble, bottom-right
    fn render_toasts(&mut self, ctx: &egui::Context) {
        let now_t = Instant::now();
        self.pending_toasts
            .retain(|(_, _, at)| now_t.duration_since(*at) < Duration::from_secs(7));
        // an undo offer ages out with the toast it belongs to
        if self
            .undo
            .as_ref()
            .map(|(_, _, at)| at.elapsed() > Duration::from_secs(7))
            .unwrap_or(false)
        {
            self.undo = None;
        }
        if !self.pending_toasts.is_empty() || self.busy.load(Ordering::Relaxed) {
            egui::Window::new("")
                .anchor(Align2::RIGHT_BOTTOM, [-12.0, -12.0])
                .resizable(false)
                .collapsible(false)
                .title_bar(false)
                // the viewer overlay is Order::Foreground; keep toasts above
                // it so action feedback is visible while a photo is open
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    self.pending_toasts
                        .drain(..)
                        .for_each(|(msg, is_error, _)| {
                            let col = if is_error { RED } else { GREEN };
                            ui.colored_label(col, truncate(&msg, 110));
                        });
                    if let Some((action, keys, _)) = self.undo.clone() {
                        if ui.button(egui::RichText::new("Undo").color(BLUE)).clicked() {
                            let sess = self.lib_session.clone();
                            let tx = self.tx.clone();
                            let act = action.clone();
                            thread::spawn(move || {
                                let Some(s) = sess.lock().unwrap().clone() else { return };
                                let ok = match act.as_str() {
                                    "restore" => library::set_trashed(&s, &keys, false),
                                    "unarchive" => library::set_archived(&s, &keys, false),
                                    "fav" => library::set_favorite(&s, &keys, true),
                                    "unfav" => library::set_favorite(&s, &keys, false),
                                    "unlock" => library::unlock_items(&s, &keys),
                                    "lock" => library::lock_items(&s, &keys),
                                    _ => false,
                                };
                                if ok {
                                    let _ = tx.send(Msg::RefreshLib);
                                }
                            });
                            self.undo = None;
                        }
                    }
                    if self.pending_toasts.is_empty() && self.busy.load(Ordering::Relaxed) {
                        ui.colored_label(INK_SOFT, "pencils down, working on it…");
                    }
                });
        }
    }
}

// side storage for drained messages lives in App.pending_toasts

// thumbnails live in the OS cache dir so each one is ever
// downloaded once, even across reboots
// on/off for every disk cache; worker threads read it without a handle
static CACHE_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn cache_on() -> bool {
    CACHE_ON.load(Ordering::Relaxed)
}

// where you stopped in each video: key<TAB>seconds, one per line
fn resume_file() -> PathBuf {
    config_dir().join("resume.tsv")
}

fn load_resume() -> std::collections::HashMap<String, f64> {
    let mut map = std::collections::HashMap::new();
    if let Ok(text) = std::fs::read_to_string(resume_file()) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('\t') {
                if let Ok(secs) = v.trim().parse::<f64>() {
                    map.insert(k.trim().to_string(), secs);
                }
            }
        }
    }
    map
}

fn save_resume(map: &std::collections::HashMap<String, f64>) {
    let p = resume_file();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut rows: Vec<String> = map
        .iter()
        .filter(|(_, v)| **v > 1.0)
        .map(|(k, v)| format!("{}\t{}", k, v.round() as i64))
        .collect();
    rows.sort();
    // shortcut: a few hundred remembered files, no eviction; add an LRU cap
    // when somebody watches more films than that
    let _ = std::fs::write(p, rows.join("\n") + "\n");
}

fn settings_file() -> PathBuf {
    config_dir().join("settings.conf")
}

// settings.conf is a tiny key=value file; one line per setting
fn setting(key: &str, default: &str) -> String {
    std::fs::read_to_string(settings_file())
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == key))
                .map(|(_, v)| v.trim().to_string())
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

// Safe, unique filename for a bulk original download. Uses the real name when
// the library scan has one; never lets an item overwrite another in the batch.
fn export_filename(raw: &str, key: &str, is_video: bool, used: &mut std::collections::HashSet<String>) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if name.chars().all(|c| c == '.') {
        name.clear();
    }
    if !name.contains('.') {
        name.push_str(if is_video { ".mp4" } else { ".jpg" });
    }
    // a short key hash stops two different photos with the same filename from
    // reading a previous batch's cached bytes under the same path
    let tag = format!("{:06x}", crc32(key.as_bytes()) & 0xfff_fff);
    let (stem, ext) = name
        .rsplit_once('.')
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .unwrap_or((name.clone(), String::new()));
    let mut unique = format!("{}_{}.{}", stem, tag, ext);
    let mut n = 2;
    while used.contains(&unique) {
        unique = format!("{}_{}_{}.{}", stem, tag, n, ext);
        n += 1;
    }
    used.insert(unique.clone());
    unique
}

fn set_setting(key: &str, val: &str) {
    let p = settings_file();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut lines: Vec<String> = std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with(&format!("{key}=")))
        .map(String::from)
        .collect();
    lines.push(format!("{key}={val}"));
    // selected_email/selected_jar are persisted here — never 0644
    let _ = write_secret_file(&p, &(lines.join("\n") + "\n"));
}

fn load_cache_flag() -> bool {
    let on = setting("cache", "1") != "0";
    CACHE_ON.store(on, Ordering::Relaxed);
    on
}

fn save_cache_flag(on: bool) {
    CACHE_ON.store(on, Ordering::Relaxed);
    set_setting("cache", if on { "1" } else { "0" });
}

// one bordered panel on the settings page, so it reads as sections
// every key the app answers to, in one card, reachable with `?`
fn keys_card(ctx: &egui::Context) {
    let rows: [(&str, &[(&str, &str)]); 3] = [
        (
            "Anywhere",
            &[
                ("?", "this card"),
                ("g", "jump to a date"),
                ("ctrl+click", "select a tile (or its corner circle)"),
                ("ctrl+a", "select all items in the grid"),
                ("ctrl+shift+a", "clear the selection"),
                ("shift+click", "select a range"),
                ("delete", "trash the selection"),
                ("f", "favourite / unfavourite the selection"),
                ("e", "archive / unarchive the selection"),
                ("ctrl+wheel", "thumbnail size (grid)"),
                ("esc", "close the viewer/card, clear the selection"),
            ],
        ),
        (
            "Photo viewer",
            &[
                ("left · right", "previous / next"),
                ("home / end", "first / last"),
                ("+ · -", "zoom in / out"),
                ("0", "fit to window"),
                ("i", "details"),
                ("s", "start / stop slideshow"),
                ("f", "fullscreen"),
            ],
        ),
        (
            "Video",
            &[
                ("space · k", "play / pause"),
                ("left · right", "10 seconds back / forward"),
                ("up · down", "a minute back / forward"),
                ("j · l", "10 seconds back / forward"),
                ("0 − 9", "jump to that tenth"),
                ("m", "mute · unmute"),
                ("f", "fullscreen"),
            ],
        ),
    ];
    egui::Window::new(egui::RichText::new("Keyboard").size(17.0).strong().color(INK))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .title_bar(true)
        .frame(
            egui::Frame::none()
                .fill(PAPER)
                .stroke(Stroke::new(1.5, INK))
                .rounding(egui::Rounding::same(10.0))
                .inner_margin(egui::Margin::same(16.0)),
        )
        .show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                for (title, keys) in rows {
                    ui.vertical(|ui| {
                        ui.set_width(210.0);
                        ui.label(egui::RichText::new(title).size(14.5).strong().color(INK));
                        ui.add_space(6.0);
                        for (k, what) in keys {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(*k)
                                        .size(12.5)
                                        .strong()
                                        .color(INK)
                                        .background_color(YELLOW.gamma_multiply(0.45)),
                                );
                                ui.label(
                                    egui::RichText::new(*what).size(12.5).color(INK_SOFT),
                                );
                            });
                            ui.add_space(3.0);
                        }
                    });
                    ui.add_space(14.0);
                }
            });
        });
}

fn settings_card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    // style-driven native frame: one place restyles every settings card
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
        ui.heading(title);
        ui.add_space(2.0);
        body(ui);
    });
}

fn video_cache_dir() -> PathBuf {
    cache_dir().join("videos")
}

// a video already downloaded, so seeking reads the disk instead of the cdn
// pull the whole stream down once; later plays and seeks use the file
fn cache_video_async(key: &str, url: &str) {
    if !cache_on() || url.is_empty() || url.starts_with('/') || cached_video(key).is_some() {
        return;
    }
    static BUSY: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    {
        let mut busy = BUSY.lock().unwrap();
        let set = busy.get_or_insert_with(HashSet::new);
        if !set.insert(key.to_string()) {
            return;
        }
    }
    let (key, url) = (key.to_string(), url.to_string());
    thread::spawn(move || {
        let dir = video_cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let final_path = dir.join(format!("{}.mp4", key));
        let part = dir.join(format!("{}.part", key));
        // the signed =dv URL carries an auth token in its query — keep it off
        // the command line via a curl config on stdin (no cookie needed here)
        let cfg = format!(
            "url = {}\nlocation = true\nmax-time = 1800\nuser-agent = \"Mozilla/5.0 Chrome/131.0\"\noutput = {}\n",
            library::curl_quote(&url),
            library::curl_quote(&part.to_string_lossy()),
        );
        let ok = {
            use std::io::Write;
            let mut child = Command::new("curl")
                .arg("-s")
                .arg("-K")
                .arg("-")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let mut success = false;
            if let Ok(ref mut c) = child {
                if let Some(mut inp) = c.stdin.take() {
                    let _ = inp.write_all(cfg.as_bytes());
                }
                success = c.wait().map(|s| s.success()).unwrap_or(false);
            }
            success
        } && std::fs::metadata(&part).map(|m| m.len() > 4096).unwrap_or(false);
        if ok {
            let _ = std::fs::rename(&part, &final_path);
        } else {
            let _ = std::fs::remove_file(&part);
        }
        if let Some(set) = BUSY.lock().unwrap().as_mut() {
            set.remove(&key);
        }
    });
}

fn cached_video(key: &str) -> Option<std::path::PathBuf> {
    let p = video_cache_dir().join(format!("{}.mp4", key));
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

fn dir_size(p: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            // .part files are in-flight downloads, not reclaimable cache
            if path.extension().and_then(|x| x.to_str()) == Some("part") {
                continue;
            }
            if let Ok(md) = e.metadata() {
                total += if md.is_dir() { dir_size(&path) } else { md.len() };
            }
        }
    }
    total
}

// wipe every disk cache the toggle controls
fn clear_caches() {
    for dir in [thumb_cache_dir(), full_cache_dir(), video_cache_dir()] {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                // .part files are in-flight downloads — deleting one makes the
                // worker report a failure for no reason (same rule as dir_size)
                if p.extension().and_then(|x| x.to_str()) == Some("part") {
                    continue;
                }
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(&p);
                } else {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
}

// egui's horizontal layout gives a label infinite width, so wrap the long
// report lines ourselves to keep them inside the card
fn wrap_line(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for word in s.split_whitespace() {
        if col > 0 && col + 1 + word.len() > width {
            out.push('\n');
            col = 0;
        } else if col > 0 {
            out.push(' ');
            col += 1;
        }
        out.push_str(word);
        col += word.len();
    }
    out
}

fn human_gb(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.2} GB", bytes as f64 / 1_073_741_824.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    }
}

fn thumb_cache_dir() -> PathBuf {
    cache_dir().join("thumbs")
}

fn send_thumb(tx: &mpsc::Sender<Msg>, key: &str, bytes: &[u8]) -> bool {
    let Some(img) = image::load_from_memory(bytes).ok().map(|i| i.to_rgba8()) else {
        return false;
    };
    let (w, h) = img.dimensions();
    let _ = tx.send(Msg::Thumb(
        key.to_string(),
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw()),
    ));
    true
}

// native pipeline: the list rpc already carries each item's fife thumb url;
// fetching it needs the fife-scoped cookie header (fifecookies.py) and then
// it is a plain https get — 8 workers, disk cache first
fn fetch_thumbs_for(s: &library::Session, jobs: &[(String, String)], tx: &mpsc::Sender<Msg>) {
    let dir = thumb_cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    // failed keys are remembered for this pass: a dead CDN must not make the
    // 5-second fill loop re-queue the same 120 URLs forever
    // failed keys with the time of the failure: a dead CDN must not be
    // hammered forever, but a key that failed once must get a retry after a
    // cooldown — permanent failure left visible tiles blank all session
    static FAILED: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Instant>>> =
        std::sync::OnceLock::new();
    let failed = FAILED.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    {
        let mut f = failed.lock().unwrap();
        // the item set changed — clear old failures so new items get a try
        let keys: HashSet<String> = jobs.iter().map(|(k, _)| k.clone()).collect();
        f.retain(|k, _| keys.contains(k));
        // retry after 60 s instead of never
        f.retain(|_, at| at.elapsed() < std::time::Duration::from_secs(60));
    }
    // fresh header every pass: brave rotates its session timestamps, so a
    // hours-old header would slowly 403
    let cookie = library::fife_cookie_header();
    let save = cache_on();
    let todo: Vec<(String, String)> = jobs
        .iter()
        .filter(|(k, u)| {
            !u.is_empty() && !dir.join(k).exists() && !failed.lock().unwrap().contains_key(k)
        })
        .cloned()
        .collect();
    for chunk in todo.chunks(16) {
        let tx = tx.clone();
        let dir2 = dir.clone();
        thread::scope(|sc| {
            for (key, url) in chunk {
                let tx = tx.clone();
                let cookie = &cookie;
                let dir2 = &dir2;
                sc.spawn(move || {
                    let Some(bytes) = fetch_thumb(url, cookie) else {
                        failed.lock().unwrap().insert(key.clone(), Instant::now());
                        return;
                    };
                    if save {
                        let part = dir2.join(format!("{}.part", key));
                        if std::fs::write(&part, &bytes).is_ok() {
                            let _ = std::fs::rename(&part, dir2.join(key));
                        }
                    }
                    send_thumb(&tx, key, &bytes);
                });
            }
        });
    }
    // serve earlier-session cached thumbs that RAM lacks. Freshly fetched
    // keys were just sent by the workers above — skip them to avoid decoding
    // and inserting every new thumbnail twice.
    let fresh: std::collections::HashSet<&str> =
        todo.iter().map(|(k, _)| k.as_str()).collect();
    for (key, _) in jobs {
        if fresh.contains(key.as_str()) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(dir.join(key)) {
            if !send_thumb(tx, key, &bytes) {
                // corrupt/partial cache entry: drop it and repair from source
                let _ = std::fs::remove_file(dir.join(key));
                if let Some(url) = jobs.iter().find(|(k, _)| k == key).map(|(_, u)| u.clone()) {
                    if let Some(fresh) = fetch_thumb(&url, &cookie) {
                        if save {
                            let part = dir.join(format!("{}.part", key));
                            if std::fs::write(&part, &fresh).is_ok() {
                                let _ = std::fs::rename(&part, dir.join(key));
                            }
                        }
                        send_thumb(tx, key, &fresh);
                    }
                }
            }
        }
    }
}

// small =w160 thumbnail fetch for the library grid
fn fetch_thumb(url: &str, cookie: &str) -> Option<Vec<u8>> {
    fetch_media(url, cookie, "=w256")
}

// any fife representation of a photo: =w160 thumbs, =w2048 viewer, =d original
fn image_mime(b: &[u8]) -> &'static str {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if b.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if b.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "application/octet-stream"
    }
}

// Clipboard images are handed to the OS as pixels, so no custom MIME and no
// external xclip/timeout helper is needed on any platform.
fn copy_bytes_to_clipboard(bytes: &[u8]) -> bool {
    let Ok(decoded) = image::load_from_memory(bytes) else {
        return false;
    };
    let rgba = decoded.to_rgba8();
    let Some(clip) = clipboard() else {
        return false;
    };
    let mut cb = clip.lock().unwrap_or_else(|e| e.into_inner());
    cb.set_image(arboard::ImageData {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        bytes: rgba.into_raw().into(),
    })
    .is_ok()
}

fn fetch_media(url: &str, cookie: &str, suffix: &str) -> Option<Vec<u8>> {
    let sized = sized_url(url, suffix);
    let out = library::curl_with_cookie(
        &[
            format!("url = {}", library::curl_quote(&sized)),
            "location = true".into(),
            "fail = true".into(),
            "max-time = 120".into(),
            format!(
                "user-agent = {}",
                library::curl_quote("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            ),
        ],
        cookie,
    )?;
    if !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

// ---- in-app video player ---------------------------------------------------
// ffmpeg is the same decoder mpv/vlc/iina use under the hood; here it streams
// into raw frames we upload as a texture, and a second pipe feeds the audio
// device through cpal. A decoder can also be started while the pointer hovers.
struct Prewarm {
    key: String,
    url: String,
    child: Arc<Mutex<Option<std::process::Child>>>,
    slots: Slots,
    w: usize,
    h: usize,
}

struct Vid {
    key: String,
    url: String,
    dur: f64,
    offset: f64,
    frames: Arc<AtomicU64>,
    // frames counted before this playback segment started: needed because a
    // prewarmed decoder is adopted mid-flight
    frames_base: u64,
    // when this segment started, so the clock can never outrun real time
    t0: Option<Instant>,
    // waiting for the first frame after a seek
    seeking: bool,
    eof: Arc<AtomicBool>,
    fps: f64,
    playing: bool,
    ended: bool,
    tex: Option<egui::TextureHandle>,
    size: (usize, usize),
    child: Arc<Mutex<Option<std::process::Child>>>,
    // dropping the cpal stream stops the device callback (and its writer)
    audio: Arc<Mutex<Option<cpal::Stream>>>,
    adec: Arc<Mutex<Option<std::process::Child>>>,
    frame: Arc<Mutex<Option<egui::ColorImage>>>,
    gen: Arc<AtomicU64>,
    // audio is the master clock: bytes actually handed to the device, the
    // epoch guarding that counter, and where this epoch starts in the film
    aplay: Arc<AtomicU64>,
    // 0 means "no audio clock"; every segment gets a fresh, never-reused epoch
    // so a retired callback cannot CAS a later segment back to 0
    audio_epoch: Arc<AtomicU64>,
    // only source of epoch values, rises monotonically (UI thread only)
    audio_id: u64,
    audio_base: f64,
    // slider position while dragging, before the seek lands
    preview: Option<f64>,
    src: (u32, u32),
    muted: bool,
    looping: bool,
}

impl Vid {
    fn new(key: String, url: String, dur: f64, src: (u32, u32)) -> Self {
        Self {
            key,
            url,
            dur,
            offset: 0.0,
            frames: Arc::new(AtomicU64::new(0)),
            frames_base: 0,
            t0: None,
            seeking: false,
            eof: Arc::new(AtomicBool::new(false)),
            fps: 25.0,
            playing: false,
            ended: false,
            tex: None,
            size: (16, 9),
            child: Arc::new(Mutex::new(None)),
            audio: Arc::new(Mutex::new(None)),
            adec: Arc::new(Mutex::new(None)),
            frame: Arc::new(Mutex::new(None)),
            gen: Arc::new(AtomicU64::new(0)),
            aplay: Arc::new(AtomicU64::new(0)),
            audio_epoch: Arc::new(AtomicU64::new(0)),
            audio_id: 0,
            audio_base: 0.0,
            preview: None,
            src,
            muted: std::env::var("GPHOTOS_MUTE").is_ok(),
            looping: false,
        }
    }

    fn pos(&self) -> f64 {
        if let Some(p) = self.preview {
            return p;
        }
        let n = self
            .frames
            .load(Ordering::Relaxed)
            .saturating_sub(self.frames_base) as f64
            / self.fps;
        // ffmpeg dumps a burst of frames in the first moment after a seek (it
        // fills its read buffer at full speed before -re pacing kicks in). The
        // wall clock is what the viewer actually sees, so cap by it — that is
        // the "first second plays at 2x" bug.
        let wall = self
            .t0
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(n);
        if self.audio_epoch.load(Ordering::Relaxed) != 0 {
            // sound is the master: picture follows the samples already fed to
            // the speaker. cap by wall time so an audio buffer burst can't run
            // the playhead ahead of what you actually hear.
            let a = self.aplay.load(Ordering::Relaxed) as f64 / AUDIO_RATE;
            return self.offset + self.audio_base + a.min(wall);
        }
        self.offset + n.min(wall)
    }
}

// the four shared handles a decoder thread talks through, so a prewarmed
// decoder can be handed straight to the viewer
#[derive(Clone)]
struct Slots {
    frames: Arc<AtomicU64>,
    eof: Arc<AtomicBool>,
    frame: Arc<Mutex<Option<egui::ColorImage>>>,
    gen: Arc<AtomicU64>,
    aplay: Arc<AtomicU64>,
    audio_epoch: Arc<AtomicU64>,
}

impl Slots {
    fn new() -> Self {
        Self {
            frames: Arc::new(AtomicU64::new(0)),
            eof: Arc::new(AtomicBool::new(false)),
            frame: Arc::new(Mutex::new(None)),
            gen: Arc::new(AtomicU64::new(0)),
            aplay: Arc::new(AtomicU64::new(0)),
            audio_epoch: Arc::new(AtomicU64::new(0)),
        }
    }
}

// tiny deterministic hash for grouping identical filenames without a crate
fn crc32(bytes: &[u8]) -> u64 {
    let mut c = 0xffff_ffffu64;
    for &b in bytes {
        c ^= b as u64;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
        }
    }
    !c & 0xffff_ffff
}

// s16le, 2 channels, 48 kHz = bytes per second of audio
const AUDIO_RATE: f64 = 48_000.0 * 4.0;

// The device callback and the byte-accounting test share this helper. Only the
// PCM actually copied into the output buffer is counted; silence from an
// underrun is not, so the playhead can never get ahead of the sound you hear.
fn consume_pcm(out: &mut [u8], pcm: &[u8], clock: &AtomicU64) -> usize {
    let n = out.len().min(pcm.len());
    out[..n].copy_from_slice(&pcm[..n]);
    clock.fetch_add(n as u64, Ordering::Relaxed);
    n
}

// ffmpeg -> cpal hand-off. The decoder fills `pcm`; the device callback drains
// it and does the clock accounting. `done` marks decoder EOF.
#[derive(Default)]
struct AudioQueue {
    pcm: VecDeque<u8>,
    done: bool,
}

// one second of s16le stereo: bounds the hand-off so a stalled device cannot
// make the decoder buffer the whole film in memory
const AUDIO_QUEUE_MAX: usize = 48_000 * 4;

// one helper so the rail drag and its selftest agree on the mapping
fn scroll_fraction_to_offset(frac: f32, content_h: f32, view_h: f32) -> f32 {
    let max = (content_h - view_h).max(0.0);
    frac.clamp(0.0, 1.0) * max
}

// one reader per decoder: latest frame only, no queue to fall behind, and the
// frame rate is paced HERE rather than by ffmpeg's -re. -re dumps a burst of
// frames when a seek starts, which showed up as the first second of a video
// playing at double speed. Holding each frame until its moment fixes that and
// keeps the playhead, the picture and the wall clock on the same line.
fn spawn_reader(
    mut out: std::process::ChildStdout,
    w: usize,
    h: usize,
    slots: Slots,
    gen: u64,
    pace: (Instant, f64),
) {
    thread::spawn(move || {
        use std::io::Read;
        let mut buf = vec![0u8; w * h * 4];
        let (t0, fps) = pace;
        let mut n: u64 = 0;
        let mut seen_epoch = 0u64;
        let mut n_base = 0u64;
        loop {
            if slots.gen.load(Ordering::Relaxed) != gen {
                break;
            }
            if out.read_exact(&mut buf).is_err() {
                if slots.gen.load(Ordering::Relaxed) == gen {
                    slots.eof.store(true, Ordering::Relaxed);
                }
                break;
            }
            n += 1;
            if slots.gen.load(Ordering::Relaxed) != gen {
                break;
            }
            let mut epoch = slots.audio_epoch.load(Ordering::Relaxed);
            if epoch != seen_epoch {
                // the audio clock changed (mute, seek, or audio EOF). Rebase to
                // where the sound actually is, not frame 1 — otherwise with an
                // old t0 every due time is in the past and the picture races
                // ahead or drops thousands of frames after a long playback.
                let a = slots.aplay.load(Ordering::Relaxed) as f64 / AUDIO_RATE;
                seen_epoch = epoch;
                n_base = n.saturating_sub((a * fps).round() as u64).min(n - 1);
                if std::env::var("GPHOTOS_VDBG").is_ok() {
                    eprintln!("reader clock epoch={} n={} rebase={}", epoch, n, n_base);
                }
            }
            let audio_mode = epoch != 0;
            let mut target = (n - n_base) as f64 / fps;
            let mut due = t0 + Duration::from_secs_f64(target);
            if audio_mode {
                let a = slots.aplay.load(Ordering::Relaxed) as f64 / AUDIO_RATE;
                // start a little behind the sound rather than dumping a burst
                let extra = (target - a).clamp(0.0, 1.0);
                due = t0 + Duration::from_secs_f64(target + extra);
            }
            // sleep in small slices so a new generation or a mute/unmute can
            // interrupt the wait instead of writing a stale frame
            loop {
                let now = Instant::now();
                if now >= due {
                    break;
                }
                thread::sleep((due - now).min(Duration::from_millis(20)));
                if slots.gen.load(Ordering::Relaxed) != gen {
                    if std::env::var("GPHOTOS_VDBG").is_ok() {
                        eprintln!(
                            "reader stale gen={} current={} n={} dropped",
                            gen,
                            slots.gen.load(Ordering::Relaxed),
                            n
                        );
                    }
                    return;
                }
                let e2 = slots.audio_epoch.load(Ordering::Relaxed);
                if e2 != epoch {
                    let a2 = slots.aplay.load(Ordering::Relaxed) as f64 / AUDIO_RATE;
                    seen_epoch = e2;
                    n_base = n.saturating_sub((a2 * fps).round() as u64).min(n - 1);
                    epoch = e2;
                    target = (n - n_base) as f64 / fps;
                    due = if epoch != 0 {
                        let a = slots.aplay.load(Ordering::Relaxed) as f64 / AUDIO_RATE;
                        t0 + Duration::from_secs_f64(target + (target - a).clamp(0.0, 1.0))
                    } else {
                        t0 + Duration::from_secs_f64(target)
                    };
                }
            }
            if n == 1 && std::env::var("GPHOTOS_VDBG").is_ok() {
                eprintln!("reader first frame gen={} after={:?}", gen, t0.elapsed());
            }
            if epoch != 0 {
                let a = slots.aplay.load(Ordering::Relaxed) as f64 / AUDIO_RATE;
                if (a - target).abs() > 3.0 / fps {
                    // after a stall the pipe holds old frames: drain them
                    // without painting, then rejoin the audio clock
                    if std::env::var("GPHOTOS_VDBG").is_ok() {
                        eprintln!("reader drop n={} target={:.2} audio={:.2}", n, target, a);
                    }
                    continue;
                }
            }
            slots.frames.fetch_add(1, Ordering::Relaxed);
            *slots.frame.lock().unwrap() =
                Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &buf));
        }
    });
}

// decode no bigger than 720 on the long edge so software hevc keeps up
fn decode_size(src: (u32, u32)) -> (usize, usize) {
    let (mut w, mut h) = (src.0.max(2) as usize, src.1.max(2) as usize);
    let long = w.max(h);
    if long > 720 {
        let k = 720.0 / long as f64;
        w = ((w as f64 * k) as usize) & !1;
        h = ((h as f64 * k) as usize) & !1;
    }
    (w, h)
}

fn kill_vid_children(v: &mut Vid) {
    // 0 is the "no audio clock" sentinel the frame clock needs; the id bump
    // retires this segment for good, so its stale callback can never CAS the
    // NEXT segment's (different) epoch back to 0
    v.audio_epoch.store(0, Ordering::Relaxed);
    v.audio_id += 1;
    // dropping the stream stops the cpal callback; the epoch above retires its
    // decoder writer and the frame reader on their next tick
    v.audio.lock().unwrap().take();
    for slot in [&v.child, &v.adec] {
        if let Some(mut c) = slot.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// audio only: ffmpeg decodes pcm at the same time offset, cpal feeds the device
// ---- hover preview: a few seconds of a video, small, muted, looping ---------
// A tile that plays its own video the moment the pointer rests on it, like the
// phone app and Immich do, so you never open a video to find out what it is.
// The clip is cut once into the cache as a tiny file; later hovers play from
// disk with no network at all.

struct HoverVid {
    key: String,
    tex: Option<egui::TextureHandle>,
    slot: Arc<Mutex<Option<(u64, egui::ColorImage)>>>,
    child: Arc<Mutex<Option<std::process::Child>>>,
    last_gen: u64,
    failed: bool,
}

impl HoverVid {
    fn stop(&self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn preview_dir() -> PathBuf {
    cache_dir().join("previews")
}

// 320 px wide, even height, so the frame size is known before decoding starts
fn preview_size(w: u32, h: u32) -> (usize, usize) {
    let (w, h) = if w == 0 || h == 0 { (16, 9) } else { (w, h) };
    let pw = 320usize;
    let ph = ((pw as f64 * h as f64 / w as f64) / 2.0).round() as usize * 2;
    (pw, ph.clamp(120, 640))
}

fn start_hover_preview(key: String, thumb: String, dims: (u32, u32)) -> HoverVid {
    let (pw, ph) = preview_size(dims.0, dims.1);
    let slot = Arc::new(Mutex::new(None));
    let child = Arc::new(Mutex::new(None));
    let (slot2, child2) = (slot.clone(), child.clone());
    let clip = preview_dir().join(format!("{}.mp4", key));
    let key_for_thread = key.clone();
    thread::spawn(move || {
        let key = key_for_thread;
        // a clip we already cut is the fast path: no network, no encode
        if !clip.metadata().map(|m| m.len() > 2000).unwrap_or(false) {
            let url = cached_video(&key)
                .map(|p| p.to_string_lossy().into_owned())
                .or_else(|| {
                    // resolve the signed redirect once; ffmpeg cannot follow it
                    let target = sized_url(&thumb, "=dv");
                    let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
                    let out = library::curl_with_cookie(
                        &[
                            format!("url = {}", library::curl_quote(&target)),
                            format!("output = {}", library::curl_quote(null_device(cfg!(windows)))),
                            "write-out = \"%{redirect_url}\"".into(),
                            "max-time = 20".into(),
                            format!("user-agent = {}", library::curl_quote(ua)),
                        ],
                        &library::fife_cookie_header(),
                    )?;
                    let u = String::from_utf8_lossy(&out).trim().to_string();
                    if u.is_empty() { None } else { Some(u) }
                });
            let Some(url) = url else { return };
            let _ = std::fs::create_dir_all(preview_dir());
            let part = clip.with_extension("part.mp4");
            let ok = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
                .args(["-ss", "1", "-t", "6"])
                .arg("-i")
                .arg(&url)
                .args(["-vf", &format!("scale={}:{}", pw, ph), "-an", "-r", "8"])
                .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "30"])
                .arg(&part)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                let _ = std::fs::rename(&part, &clip);
            } else {
                let _ = std::fs::remove_file(&part);
                return;
            }
        }
        // play the clip on a loop, frames paced here so the pipe cannot burst
        let Ok(mut c) = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-stream_loop", "-1", "-i"])
            .arg(&clip)
            .args(["-vf", &format!("scale={}:{}", pw, ph), "-an", "-r", "8"])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let Some(mut out) = c.stdout.take() else { return };
        *child2.lock().unwrap() = Some(c);
        let mut buf = vec![0u8; pw * ph * 4];
        let mut gen = 0u64;
        loop {
            use std::io::Read;
            if out.read_exact(&mut buf).is_err() {
                break;
            }
            gen += 1;
            *slot2.lock().unwrap() = Some((
                gen,
                egui::ColorImage::from_rgba_unmultiplied([pw, ph], &buf),
            ));
            thread::sleep(Duration::from_millis(125));
        }
    });
    HoverVid {
        key,
        tex: None,
        slot,
        child,
        last_gen: 0,
        failed: false,
    }
}

fn spawn_audio(v: &mut Vid, url: &str, at: &str) {
    v.aplay.store(0, Ordering::Relaxed);
    // Every segment gets a fresh, never-reused epoch: a dying callback from an
    // older segment cannot consume this audio or CAS the new epoch to 0. Zero
    // stays reserved for "no audio clock" (frame-clock fallback).
    v.audio_id += 1;
    let epoch = v.audio_id;
    v.audio_epoch.store(epoch, Ordering::Relaxed);
    v.audio_base = (at.parse::<f64>().unwrap_or(0.0) - v.offset).max(0.0);
    // keep env lookups off the real-time audio thread
    let debug = std::env::var("GPHOTOS_VDBG").is_ok();
    let reset_epoch = |v: &mut Vid| {
        let _ = v.audio_epoch.compare_exchange(epoch, 0, Ordering::Relaxed, Ordering::Relaxed);
    };

    // The device callback pulls PCM and accounts the bytes it actually took;
    // ffmpeg's decoder only fills the queue. No external audio helper needed.
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        if debug {
            eprintln!("audio: no output device; video will be silent (48 kHz stereo needed)");
        }
        reset_epoch(v);
        return;
    };
    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: cpal::SampleRate(48_000),
        buffer_size: cpal::BufferSize::Default,
    };
    let queue: Arc<Mutex<AudioQueue>> = Arc::new(Mutex::new(AudioQueue::default()));
    let (ax, ex, q) = (v.aplay.clone(), v.audio_epoch.clone(), queue.clone());
    let err_ex = v.audio_epoch.clone();
    let err_logged = Arc::new(AtomicBool::new(false));
    let stream = match device.build_output_stream_raw(
        &config,
        cpal::SampleFormat::I16,
        move |data: &mut cpal::Data, _| {
            let out = data.bytes_mut();
            let Ok(mut qq) = q.try_lock() else {
                out.fill(0); // never block the real-time callback
                return;
            };
            if ex.load(Ordering::Relaxed) != epoch {
                out.fill(0); // muted, seeked or paused meanwhile
                return;
            }
            let n = consume_pcm(out, qq.pcm.make_contiguous(), &ax);
            qq.pcm.drain(..n);
            out[n..].fill(0); // underrun -> silence, deliberately uncounted
            if qq.done && qq.pcm.is_empty() {
                // audio ended naturally: fall back to the frame clock, but only
                // if no newer segment has replaced this epoch meanwhile
                let _ = ex.compare_exchange(epoch, 0, Ordering::Relaxed, Ordering::Relaxed);
                if debug {
                    eprintln!("audio exit epoch={} secs={:.2}", epoch, ax.load(Ordering::Relaxed) as f64 / AUDIO_RATE);
                }
            }
        },
        move |err| {
            // a stopped device would freeze aplay and stall the playhead, so
            // leave audio mode and let the frame clock take over
            if debug && !err_logged.swap(true, Ordering::Relaxed) {
                eprintln!("audio device error epoch={}: {}", epoch, err);
            }
            let _ = err_ex.compare_exchange(epoch, 0, Ordering::Relaxed, Ordering::Relaxed);
        },
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            if debug {
                eprintln!("audio: cannot open 48 kHz stereo output: {}", e);
            }
            reset_epoch(v);
            return;
        }
    };

    let mut acmd = Command::new("ffmpeg");
    acmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-re"]);
    if url.starts_with("http") {
        acmd.args(["-rw_timeout", "15000000"]);
    }
    let mut dec = match acmd
        .args(["-ss", at])
        .arg("-i")
        .arg(url)
        .args(["-vn", "-ac", "2", "-ar", "48000", "-f", "s16le", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            reset_epoch(v);
            return;
        }
    };
    let Some(mut from_dec) = dec.stdout.take() else {
        let _ = dec.kill();
        let _ = dec.wait();
        reset_epoch(v);
        return;
    };

    let wq = queue.clone();
    let wex = v.audio_epoch.clone();
    thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 8192];
        loop {
            if wex.load(Ordering::Relaxed) != epoch {
                break;
            }
            let n = match from_dec.read(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => break,
            };
            // -re paces ffmpeg; only a briefly stalled device makes this wait
            loop {
                let mut q = wq.lock().unwrap();
                if wex.load(Ordering::Relaxed) != epoch {
                    return;
                }
                if q.pcm.len() + n <= AUDIO_QUEUE_MAX {
                    q.pcm.extend(&buf[..n]);
                    break;
                }
                drop(q);
                thread::sleep(Duration::from_millis(10));
            }
        }
        if let Ok(mut q) = wq.lock() {
            q.done = true;
        }
    });

    if stream.play().is_err() {
        if debug {
            eprintln!("audio: output stream refused to start; video will be silent");
        }
        let _ = dec.kill();
        let _ = dec.wait();
        reset_epoch(v);
        return;
    }
    *v.audio.lock().unwrap() = Some(stream);
    *v.adec.lock().unwrap() = Some(dec);
}


// ---------- hand-drawn vector icons -----------------------------------------
// Everything the app draws for itself is a wobbly vector shape in the app's own
// ink, so the icons keep the sketchbook look instead of looking pasted on. They
// animate from state (hover, playing, muted, selected) rather than from a lottie
// file: egui has no lottie runtime, and these shapes cost no dependency at all.
// Shapes and pacing are modelled on VLC / mpv / Play's transport icons.

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

// a line with a hair of wobble, like the pen strokes elsewhere in the app
fn wobble_line(p: &egui::Painter, a: Pos2, b: Pos2, w: f32, col: Color32, seed: u32) {
    let d = b - a;
    if d.length() < 0.01 {
        return;
    }
    let normal = Vec2::new(-d.y, d.x).normalized();
    let n = 6usize;
    let mut pts = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let mut q = a + d * (i as f32 / n as f32);
        if i > 0 && i < n {
            let ph = (i as f32 * 1.9 + seed as f32 * 1.3).sin();
            q += normal * ph * 0.45;
        }
        pts.push(q);
    }
    p.add(egui::Shape::line(pts, Stroke::new(w, col)));
}

fn wobble_poly(p: &egui::Painter, pts: &[Pos2], w: f32, col: Color32, seed: u32, close: bool) {
    for i in 0..pts.len().saturating_sub(1) {
        wobble_line(p, pts[i], pts[i + 1], w, col, seed + i as u32);
    }
    if close && pts.len() > 2 {
        wobble_line(p, pts[pts.len() - 1], pts[0], w, col, seed + 99);
    }
}

fn wobble_circle(p: &egui::Painter, c: Pos2, r: f32, w: f32, col: Color32, seed: u32) {
    let n = 26;
    let mut pts = Vec::with_capacity(n);
    for i in 0..n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        let j = 1.0 + 0.03 * (i as f32 * 2.1 + seed as f32).sin();
        pts.push(c + Vec2::angled(a) * r * j);
    }
    p.add(egui::Shape::closed_line(pts, Stroke::new(w, col)));
}

fn icon_close(p: &egui::Painter, c: Pos2, hot: f32, _t: f64, col: Color32) {
    let r = 4.6 + hot * 1.3;
    let col = mix(col, RED, hot);
    wobble_line(p, c + Vec2::new(-r, -r), c + Vec2::new(r, r), 1.8, col, 3);
    wobble_line(p, c + Vec2::new(-r, r), c + Vec2::new(r, -r), 1.8, col, 7);
}

fn icon_fullscreen(p: &egui::Painter, c: Pos2, hot: f32, _t: f64, col: Color32) {
    let s = 5.0 + hot * 1.6;
    let k = 3.2;
    for (dx, dy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
        let corner = c + Vec2::new(dx * s, dy * s);
        wobble_line(p, corner, corner + Vec2::new(-dx * k, 0.0), 1.7, col, 11);
        wobble_line(p, corner, corner + Vec2::new(0.0, -dy * k), 1.7, col, 17);
    }
}

fn icon_info(p: &egui::Painter, c: Pos2, hot: f32, _t: f64, col: Color32) {
    wobble_circle(p, c, 7.4 + hot * 0.7, 1.6, col, 5);
    p.circle_filled(c + Vec2::new(0.0, -3.4), 1.15, col);
    wobble_line(p, c + Vec2::new(0.0, -0.6), c + Vec2::new(0.0, 3.8), 1.7, col, 9);
}

fn icon_download(p: &egui::Painter, c: Pos2, hot: f32, t: f64, col: Color32) {
    let bob = ((t * 3.0).sin() as f32) * hot * 1.2;
    wobble_line(p, c + Vec2::new(0.0, -6.0 + bob), c + Vec2::new(0.0, 2.6 + bob), 1.7, col, 21);
    wobble_line(p, c + Vec2::new(-3.4, -0.6 + bob), c + Vec2::new(0.0, 3.0 + bob), 1.7, col, 23);
    wobble_line(p, c + Vec2::new(3.4, -0.6 + bob), c + Vec2::new(0.0, 3.0 + bob), 1.7, col, 25);
    wobble_poly(
        p,
        &[
            c + Vec2::new(-6.0, 3.8),
            c + Vec2::new(-6.0, 6.0),
            c + Vec2::new(6.0, 6.0),
            c + Vec2::new(6.0, 3.8),
        ],
        1.7,
        col,
        27,
        false,
    );
}

fn icon_trash(p: &egui::Painter, c: Pos2, hot: f32, _t: f64, col: Color32) {
    let _ = hot;
    wobble_line(p, c + Vec2::new(-6.0, -4.0), c + Vec2::new(6.0, -4.0), 1.6, col, 31);
    wobble_line(p, c + Vec2::new(-2.2, -6.2), c + Vec2::new(2.2, -6.2), 1.6, col, 33);
    wobble_poly(
        p,
        &[
            c + Vec2::new(-4.4, -4.0),
            c + Vec2::new(-3.4, 6.0),
            c + Vec2::new(3.4, 6.0),
            c + Vec2::new(4.4, -4.0),
        ],
        1.6,
        col,
        35,
        true,
    );
    wobble_line(p, c + Vec2::new(-1.0, -1.4), c + Vec2::new(-1.4, 3.6), 1.3, col, 37);
    wobble_line(p, c + Vec2::new(1.0, -1.4), c + Vec2::new(1.4, 3.6), 1.3, col, 39);
}

fn icon_star(p: &egui::Painter, c: Pos2, hot: f32, t: f64, filled: bool, col: Color32) {
    let beat = if filled {
        1.0 + 0.04 * (t * 3.0).sin() as f32
    } else {
        1.0
    };
    let s = (1.0 + hot * 0.08) * beat;
    let outer = 7.0 * s;
    let inner = 3.0 * s;
    let mut pts = Vec::new();
    for i in 0..10 {
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
        let r = if i % 2 == 0 { outer } else { inner };
        pts.push(c + Vec2::new(a.cos() * r, a.sin() * r));
    }
    // a star is star-shaped about its centre, so a triangle fan fills it
    // exactly — no mismatched shapes and no line drawn across the middle
    if filled {
        for w in pts.windows(2) {
            p.add(egui::Shape::convex_polygon(vec![c, w[0], w[1]], col, Stroke::NONE));
        }
    }
    p.add(egui::Shape::closed_line(pts, Stroke::new(if filled { 1.4 } else { 1.6 }, col)));
}

fn icon_speaker(
    p: &egui::Painter,
    c: Pos2,
    _hot: f32,
    t: f64,
    muted: bool,
    playing: bool,
    col: Color32,
) {
    wobble_poly(
        p,
        &[
            c + Vec2::new(-6.5, -2.5),
            c + Vec2::new(-3.0, -2.5),
            c + Vec2::new(1.0, -6.0),
            c + Vec2::new(1.0, 6.0),
            c + Vec2::new(-3.0, 2.5),
            c + Vec2::new(-6.5, 2.5),
        ],
        1.5,
        if muted { mix(col, INK, 0.35) } else { col },
        41,
        true,
    );
    if muted {
        wobble_line(p, c + Vec2::new(3.4, -5.2), c + Vec2::new(8.4, 5.2), 1.9, RED, 43);
    } else {
        // two sound waves that breathe while the video is playing
        let ph = if playing {
            (t * 3.2).sin() as f32 * 0.5 + 0.5
        } else {
            0.4
        };
        for (i, rad) in [4.6f32, 7.8].iter().enumerate() {
            let a = 0.5 + 0.5 * (ph * (1.0 + i as f32 * 0.3));
            let mut pts = Vec::new();
            for k in 0..10 {
                let ang = -0.8 + 1.6 * (k as f32 / 9.0);
                pts.push(c + Vec2::new(1.2, 0.0) + Vec2::angled(ang) * *rad);
            }
            p.add(egui::Shape::line(
                pts,
                Stroke::new(1.5, col.gamma_multiply(a)),
            ));
        }
    }
}

fn icon_share(p: &egui::Painter, c: Pos2, _hot: f32, _t: f64, col: Color32) {
    // the familiar three-dot share glyph
    let s = Stroke::new(1.5, col);
    let a = c + Vec2::new(4.5, -5.0);
    let b = c + Vec2::new(-5.0, 0.0);
    let d = c + Vec2::new(4.5, 5.0);
    p.add(egui::Shape::line(vec![b, a], s));
    p.add(egui::Shape::line(vec![b, d], s));
    for q in [a, b, d] {
        p.circle_filled(q, 1.9, col);
    }
}

fn icon_loop(p: &egui::Painter, c: Pos2, hot: f32, _t: f64, active: bool, col: Color32) {
    let _ = (hot, active);
    // a plain repeat sign, not a spinning lasso
    let s = Stroke::new(1.6, col);
    p.add(egui::Shape::line(
        vec![c + Vec2::new(-6.0, -2.6), c + Vec2::new(4.5, -2.6)],
        s,
    ));
    p.add(egui::Shape::line(
        vec![
            c + Vec2::new(1.8, -5.2),
            c + Vec2::new(4.8, -2.6),
            c + Vec2::new(1.8, 0.0),
        ],
        s,
    ));
    p.add(egui::Shape::line(
        vec![c + Vec2::new(6.0, 2.6), c + Vec2::new(-4.5, 2.6)],
        s,
    ));
    p.add(egui::Shape::line(
        vec![
            c + Vec2::new(-1.8, 0.0),
            c + Vec2::new(-4.8, 2.6),
            c + Vec2::new(-1.8, 5.2),
        ],
        s,
    ));
}

fn icon_chevron(p: &egui::Painter, c: Pos2, hot: f32, dir: f32, col: Color32) {
    let w = 3.6 + hot * 0.8;
    wobble_line(p, c + Vec2::new(dir * w, -w), c + Vec2::new(-dir * w, 0.0), 1.8, col, 61);
    wobble_line(p, c + Vec2::new(-dir * w, 0.0), c + Vec2::new(dir * w, w), 1.8, col, 63);
}

fn icon_archive(p: &egui::Painter, c: Pos2, _hot: f32, _t: f64, col: Color32) {
    wobble_line(p, c + Vec2::new(-6.5, -4.0), c + Vec2::new(6.5, -4.0), 1.6, col, 71);
    wobble_poly(
        p,
        &[
            c + Vec2::new(-5.5, -4.0),
            c + Vec2::new(-5.5, 5.6),
            c + Vec2::new(5.5, 5.6),
            c + Vec2::new(5.5, -4.0),
        ],
        1.6,
        col,
        73,
        false,
    );
    wobble_line(p, c + Vec2::new(-2.6, 0.6), c + Vec2::new(2.6, 0.6), 1.4, col, 75);
}

fn icon_play_pause(p: &egui::Painter, c: Pos2, playing: bool, filled: bool, col: Color32) {
    if filled {
        p.circle_filled(c, 14.0, Color32::from_white_alpha(225));
    }
    let ink = if filled { INK } else { col };
    if playing {
        for dx in [-4.5f32, 4.5] {
            p.rect_filled(
                Rect::from_center_size(c + Vec2::new(dx, 0.0), Vec2::new(3.6, 13.0)),
                1.0,
                ink,
            );
        }
    } else {
        p.add(egui::Shape::convex_polygon(
            vec![
                c + Vec2::new(-4.2, -7.4),
                c + Vec2::new(7.2, 0.0),
                c + Vec2::new(-4.2, 7.4),
            ],
            ink,
            Stroke::NONE,
        ));
    }
}

// one icon button: hover grows the glyph and lights a soft disc under it
fn icon_btn_at(
    ui: &mut egui::Ui,
    c: Pos2,
    id: &str,
    tip: &str,
    draw: impl FnOnce(&egui::Painter, Pos2, f32, f64),
) -> bool {
    let resp = ui.interact(
        Rect::from_center_size(c, Vec2::splat(28.0)),
        egui::Id::new(id),
        egui::Sense::click(),
    );
    let hot = if resp.hovered() { 1.0 } else { 0.0 };
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        let _ = resp.clone().on_hover_text(tip);
    }
    draw(ui.painter(), c, hot, ui.ctx().input(|i| i.time));
    resp.clicked()
}

// the same button, laid out in a normal ui row instead of at a fixed point
fn icon_btn(
    ui: &mut egui::Ui,
    id: &str,
    tip: &str,
    draw: impl FnOnce(&egui::Painter, Pos2, f32, f64),
) -> bool {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), egui::Sense::hover());
    icon_btn_at(ui, rect.center(), id, tip, draw)
}

fn hhmmss(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn is_video_file(name: &str) -> bool {
    let e = name.rsplit('.').next().unwrap_or("").to_lowercase();
    matches!(
        e.as_str(),
        "mp4" | "mov" | "avi" | "webm" | "3gp" | "mkv" | "mts" | "m4v" | "mpg" | "mpeg" | "wmv"
    )
}

// fife urls are base + size suffix; strip any existing suffix first
fn sized_url(url: &str, suffix: &str) -> String {
    let base = url.split('=').next().unwrap_or(url);
    format!("{}{}", base, suffix)
}

fn same_account(a: &library::AccountRecord, b: &library::AccountRecord) -> bool {
    a.jar == b.jar && a.email == b.email
}

// Reconcile browser-discovered identities with the persisted registry for the
// switcher. Returns (rows safe to persist, new rows staged for confirmation).
// Known rows are kept verbatim so the owner's hidden/last_seen/last_ok survive a
// rescan; brand-new rows are never persisted before confirmation (Task 5).
fn merge_accounts(
    existing: &[library::AccountRecord],
    discovered: &[library::AccountRecord],
) -> (Vec<library::AccountRecord>, Vec<library::AccountRecord>) {
    let confirmed = existing.to_vec();
    let mut staged: Vec<library::AccountRecord> = vec![];
    for d in discovered {
        if confirmed.iter().any(|e| same_account(e, d))
            || staged.iter().any(|s| same_account(s, d))
        {
            continue;
        }
        staged.push(d.clone());
    }
    (confirmed, staged)
}

// The selected identity survives restarts in settings.conf. Old installs have
// only the 0..4 authuser slot, so derive (jar,email) from it once and remember.
// The sentinel is written BEFORE the blocking probe so an offline/failed first
// run does not pay that cost on every subsequent launch.
fn load_selected_identity() -> Option<(String, String)> {
    // an explicit logout must not be undone by re-detecting browser cookies
    if setting("logged_out", "0") == "1" {
        return None;
    }
    let jar = setting("selected_jar", "");
    let email = setting("selected_email", "");
    if !jar.is_empty() && !email.is_empty() {
        // a pre-rename install may have stored a short jar label; upgrade it
        // to the full id before any capture tries to look the session up
        let full = library::resolve_legacy_jar(&jar).unwrap_or_else(|| jar.clone());
        if full != jar {
            set_setting("selected_jar", &full);
        }
        return Some((full, email));
    }
    // No stored selection: try the legacy authuser slot first, and if that
    // fails (offline first run, or a session that expired before migration),
    // discover live accounts so an already-signed-in machine still opens the
    // grid instead of being stuck on the sign-in page forever.
    let n = setting("authuser", "0").parse::<u32>().unwrap_or(0);
    // A legacy capture can still yield (jar,email) even when ListAccounts
    // discovery returns nothing (e.g. the endpoint changed or is rate-limited).
    // Try it on EVERY launch with no selection, not just the first one — the
    // migration sentinel must never strand a signed-in machine on the sign-in
    // page after a transient failure.
    let from_legacy = library::Session::capture_with(n)
        .map(|s| (library::resolve_legacy_jar(&s.jar).unwrap_or(s.jar), s.email));
    if let Some(sel) = from_legacy.or_else(|| {
        library::discover_accounts()
            .into_iter()
            .find(|a| !a.hidden && !a.email.is_empty())
            .map(|a| (a.jar, a.email))
    }) {
        set_setting("selected_jar", &sel.0);
        set_setting("selected_email", &sel.1);
        return Some(sel);
    }
    None
}

// Every worker captures through the selected identity, never the old 0..4 slot.
fn capture_selected(sel: &Option<(String, String)>) -> Option<library::Session> {
    let (jar, email) = sel.as_ref()?;
    library::Session::capture_identity(jar, email)
}

// A cached session belongs to exactly one (jar, email). Refuse to RPC with a
// session left behind by a previous account — the epoch checks alone cannot
// catch a worker that snapshotted the new epoch before the cache was cleared.
fn session_matches(sel: &Option<(String, String)>, s: &library::Session) -> bool {
    sel.as_ref().map(|(j, e)| s.jar == *j && s.email == *e).unwrap_or(false)
}

// full-size viewer images live under the OS cache dir
// accounts the user x-ed out of the accounts list
fn hidden_path() -> PathBuf {
    config_dir().join("hidden_accounts.txt")
}

// A bare email is an all-profiles sentinel from the old email-keyed file.
fn is_hidden(hidden: &HashSet<AccountKey>, jar: &str, email: &str) -> bool {
    hidden.contains(&(jar.to_string(), email.to_string()))
        || hidden.contains(&(String::new(), email.to_string()))
}

// Mapped keys are "jar<TAB>email"; sentinels stay as bare emails so a rewrite
// cannot lose the owner's old hide-everywhere choice.
fn hidden_lines(keys: &HashSet<AccountKey>) -> Vec<String> {
    keys.iter()
        .map(|(j, e)| if j.is_empty() { e.clone() } else { format!("{j}\t{e}") })
        .collect()
}

fn write_hidden(keys: &HashSet<AccountKey>) {
    // the hidden list holds account emails — keep it owner-only
    let _ = write_secret_file(&hidden_path(), &hidden_lines(keys).join("\n"));
}

// Old files held bare emails, which cannot distinguish two profiles sharing
// one email. Expand each legacy line against the registry (keyed by jar+email)
// once; emails with no registry row stay as all-profiles sentinels.
fn parse_hidden(text: &str, rows: &[library::AccountRecord]) -> (HashSet<AccountKey>, bool) {
    let mut out = HashSet::new();
    let mut migrated = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some((jar, email)) = line.split_once('\t') {
            if !jar.is_empty() && !email.is_empty() {
                out.insert((jar.to_string(), email.to_string()));
            }
        } else {
            migrated = true;
            let mut mapped = false;
            for r in rows.iter().filter(|r| r.email == line) {
                mapped = true;
                out.insert((r.jar.clone(), r.email.clone()));
            }
            // No registry row yet (normal on a legacy upgrade): keep the old
            // "hide this email everywhere" choice instead of dropping it.
            if !mapped {
                out.insert((String::new(), line.to_string()));
            }
        }
    }
    (out, migrated)
}

fn read_hidden() -> HashSet<AccountKey> {
    let text = match fs::read_to_string(hidden_path()) {
        Ok(t) => t,
        Err(_) => return HashSet::new(),
    };
    let (keys, migrated) = parse_hidden(&text, &library::load_registry());
    if migrated {
        write_hidden(&keys);
    }
    keys
}

// Only an explicit Add writes a new registry row; selecting ("use") a staged
// identity must leave the registry untouched.
fn confirm_account(
    rows: &[library::AccountRecord],
    add: &library::AccountRecord,
) -> Vec<library::AccountRecord> {
    let mut out = rows.to_vec();
    if !out.iter().any(|r| same_account(r, add)) {
        let mut row = add.clone();
        row.hidden = false;
        out.push(row);
    }
    out
}

// Windows 127+ ABE blocks external cookie decryption. A small local extension
// writes the signed-in profile's cookies here; the app imports them into the
// same 0600 session cache + registry. Format:
// {"version":1,"accounts":[{"jar":"...","email":"...","cookie":"...",
// "fife_cookie":"..."}]} — fife_cookie/version are optional. Nothing in a
// rejected file is ever printed; errors are deliberately content-free.
const MAX_COOKIE_EXPORT_BYTES: usize = 256 * 1024;
const MAX_COOKIE_EXPORT_ROWS: usize = 100;

fn parse_cookie_export(text: &str) -> Result<Vec<library::ImportedSession>, ()> {
    if text.is_empty() || text.len() > MAX_COOKIE_EXPORT_BYTES {
        return Err(());
    }
    let v: serde_json::Value = serde_json::from_str(text).map_err(|_| ())?;
    if let Some(ver) = v.get("version") {
        if ver.as_i64() != Some(1) {
            return Err(());
        }
    }
    let rows = v
        .get("accounts")
        .and_then(serde_json::Value::as_array)
        .ok_or(())?;
    if rows.is_empty() || rows.len() > MAX_COOKIE_EXPORT_ROWS {
        return Err(());
    }
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let get = |k: &str| r.get(k).and_then(serde_json::Value::as_str).unwrap_or("").trim();
        let (jar, email, cookie, fife) = (get("jar"), get("email"), get("cookie"), get("fife_cookie"));
        // jar is a stable label chosen by the user/extension, not a filesystem path
        if jar.is_empty() || jar.len() > 200 || jar.chars().any(char::is_control) {
            return Err(());
        }
        if email.len() > 320
            || !email.contains('@')
            || email.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(());
        }
        // cookie headers are single-line name=value pairs: CR/LF/control chars
        // are rejected here, not just escaped later by curl_quote
        if cookie.is_empty()
            || cookie.len() > 64 * 1024
            || !cookie.contains('=')
            || cookie.chars().any(char::is_control)
        {
            return Err(());
        }
        if fife.len() > 64 * 1024 || fife.chars().any(char::is_control) {
            return Err(());
        }
        out.push(library::ImportedSession {
            jar: jar.into(),
            email: email.into(),
            cookie: cookie.into(),
            fife_cookie: fife.into(),
        });
    }
    Ok(out)
}

// Read, validate and apply an export. Returns only a count on success; on any
// failure the caller gets a content-free error so secrets cannot reach a log.
fn import_cookie_file(path: &std::path::Path) -> Result<usize, ()> {
    let text = fs::read_to_string(path).map_err(|_| ())?;
    let rows = parse_cookie_export(&text)?;
    if !library::Session::import_sessions(&rows) {
        return Err(());
    }
    let mut registry = library::load_registry();
    for r in &rows {
        registry = confirm_account(
            &registry,
            &library::AccountRecord {
                jar: r.jar.clone(),
                email: r.email.clone(),
                name: String::new(),
                hidden: false,
                last_seen: 0,
                last_ok: 0,
            },
        );
    }
    if library::save_registry(&registry) {
        Ok(rows.len())
    } else {
        Err(())
    }
}

fn full_cache_dir() -> PathBuf {
    cache_dir().join("full")
}

fn window_icon() -> Option<egui::IconData> {
    let png = include_bytes!("logo.png");
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    Some(egui::IconData {
        width: w,
        height: h,
        rgba: img.into_raw(),
    })
}


// ---- self test -------------------------------------------------------------
// `gphotos --selftest` exercises every action the viewer buttons use on the
// newest items (today's screenshots), checking the server agrees each time
// live mutation checks read back through Google's listings, which are
// eventually consistent. Poll briefly instead of failing on the first miss.
fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for i in 0..5 {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(900));
        }
        if f() {
            return true;
        }
    }
    false
}

// offline checks: always run, no browser session or network required
fn selftest_pure() -> i32 {
    let mut fails = 0;
    let mut n = 0;
    let mut check = |name: &str, ok: bool| {
        println!("{}  {}", if ok { "ok  " } else { "FAIL" }, name);
        n += 1;
        if !ok { fails += 1; }
    };
    check(
        "civil/days round trip",
        library::civil_from_days(library::days_from_civil(2020, 2, 29)) == (2020, 2, 29)
            && library::civil_from_days(0) == (1970, 1, 1),
    );
    check(
        "date query slash / full month / dot forms",
        parse_date_query("2019/07/15")
            == Some(library::days_from_civil(2019, 7, 15) * 86400)
            && parse_date_query("July 2019")
                == Some(library::days_from_civil(2019, 7, 1) * 86400)
            && parse_date_query("2019.07")
                == Some(library::days_from_civil(2019, 7, 1) * 86400),
    );
    check(
        "date label fixed + empty",
        date_label(library::days_from_civil(2019, 7, 15) * 86400) == "15 Jul 2019"
            && date_label(0).is_empty()
            && date_label(-1).is_empty(),
    );
    // CR/LF are dropped on purpose (header-injection defence); quote, backslash
    // and tab are escaped for curl's config parser
    check(
        "curl quote escaping",
        library::curl_quote("a\"b\\c\nd\te") == "\"a\\\"b\\\\cd\\te\""
            && !library::curl_quote("x\r\ny").contains('\n'),
    );
    check(
        "video decode size is even and capped",
        decode_size((1920, 1080)) == (720, 404) && decode_size((1, 1)) == (2, 2),
    );
    check(
        "preview size bounds",
        preview_size(1920, 1080) == (320, 180) && preview_size(1920, 10000).1 <= 640,
    );
    check(
        "playhead formatting",
        hhmmss(3661.9) == "1:01:01" && hhmmss(65.0) == "1:05" && hhmmss(-5.0) == "0:00",
    );
    check(
        "sized url replaces suffix",
        sized_url("https://lh3.googleusercontent.com/p/abc=w400", "=w1200")
            == "https://lh3.googleusercontent.com/p/abc=w1200",
    );
    check(
        "image mime sniffing",
        image_mime(&[0x89, b'P', b'N', b'G']) == "image/png"
            && image_mime(&[0xFF, 0xD8]) == "image/jpeg"
            && image_mime(b"????") == "application/octet-stream",
    );
    check(
        "video file detection",
        is_video_file("CLIP.MP4") && !is_video_file("pic.JPG") && !is_video_file("noext"),
    );
    check(
        "truncate keeps char count",
        truncate("abcdef", 4) == "abcd…" && truncate("abc", 4) == "abc",
    );
    check("month name wraps", month_name(13) == "Jan" && month_name(1) == "Jan");
    check("wrap line", wrap_line("one two three", 7) == "one two\nthree");
    check(
        "manual date parse valid + invalid",
        library::parse_ymd("2015-07-15")
            == Some(library::days_from_civil(2015, 7, 15) * 86400)
            && library::parse_ymd("2015-02-30").is_none()
            && library::parse_ymd("abcd-01-01").is_none()
            && library::parse_ymd("2015/07/15").is_none(),
    );
    check(
        "filename date uses shared civil math",
        library::parse_date_from_filename("IMG_20150715_123000.jpg")
            == Some(library::days_from_civil(2015, 7, 15) * 86400 + 12 * 3600 + 30 * 60),
    );
    println!("{} pure checks, {} failed", n, fails);
    fails
}

fn selftest() -> i32 {
    let Some(s) = library::Session::capture_with(0) else {
        println!("FAIL  no session (sign in with the browser first)");
        return 1;
    };
    println!("session  {}", s.email);
    let (items, _) = library::recent(&s, None);
    if items.is_empty() {
        println!("FAIL  library came back empty");
        return 1;
    }
    println!("library  {} items", items.len());
    let today = today_start();
    let mut today_items: Vec<&library::MediaItem> =
        items.iter().filter(|i| i.ts >= today).collect();
    if today_items.len() < 4 {
        today_items = items.iter().take(8).collect();
        println!("note     fewer than 4 items from today, using the newest 8");
    }
    let mut fails = 0;
    let mut total = 0;
    let mut check = |name: &str, ok: bool| {
        println!("{}  {}", if ok { "ok  " } else { "FAIL" }, name);
        total += 1;
        if !ok {
            fails += 1;
        }
    };

    // -- the playhead must never outrun the wall clock: ffmpeg dumps a burst of
    // frames right after a seek, which used to make the first second play at 2x
    {
        let mut v = Vid::new("clock".into(), String::new(), 60.0, (1280, 720));
        v.frames.store(250, Ordering::Relaxed); // ~10 s of frames, instantly
        v.t0 = Some(Instant::now());
        let p = v.pos();
        check("playhead capped by real time after a seek burst", p < 0.5);

        // with sound on, the samples fed to the speaker are the master clock
        let mut av = Vid::new("audio-clock".into(), String::new(), 60.0, (1280, 720));
        av.audio_epoch.store(1, Ordering::Relaxed);
        av.aplay.store((AUDIO_RATE * 5.0) as u64, Ordering::Relaxed);
        av.t0 = Some(Instant::now() - Duration::from_secs(6));
        check("audio clock drives the playhead", (av.pos() - 5.0).abs() < 0.1);
        av.t0 = Some(Instant::now());
        check("audio clock still capped by wall time", av.pos() < 0.5);
        av.audio_epoch.store(0, Ordering::Relaxed);
        check("muted falls back to the frame clock", av.pos() < 0.5);

        // unmuting mid-film: the audio epoch starts where the film was
        let mut uv = Vid::new("unmute".into(), String::new(), 60.0, (1280, 720));
        uv.offset = 10.0;
        uv.audio_base = 5.0;
        uv.audio_epoch.store(1, Ordering::Relaxed);
        uv.t0 = Some(Instant::now() - Duration::from_secs(20));
        check("unmute keeps the film position", (uv.pos() - 15.0).abs() < 0.1);
    }

    // Live mutation checks (favourite/archive/trash) were removed: they mutate
    // the signed-in account's real data and Google's listings are eventually
    // consistent, so they failed randomly. Offline and read-only checks below
    // are deterministic and safe for any user to run.
    println!("note     live mutation checks skipped (would alter your library)");

    // -- local filename grouping hash is stable and distinguishes names
    check("crc32 stable", crc32(b"IMG_1234.JPG") == crc32(b"IMG_1234.JPG"));
    check("crc32 differs", crc32(b"IMG_1234.JPG") != crc32(b"IMG_4321.JPG"));
    check("crc32 empty", crc32(b"") == 0);

    // -- direct scrollbar mapping: bottom of the rail reaches the last row
    check(
        "scroll fraction 0 is the top",
        scroll_fraction_to_offset(0.0, 43784.0, 564.0) == 0.0,
    );
    check(
        "scroll fraction 1 reaches the bottom",
        (scroll_fraction_to_offset(1.0, 43784.0, 564.0) - 43220.0).abs() < 0.5,
    );
    check(
        "scroll fraction is clamped",
        scroll_fraction_to_offset(1.5, 43784.0, 564.0)
            == scroll_fraction_to_offset(1.0, 43784.0, 564.0),
    );
    check(
        "no scroll range when content fits",
        scroll_fraction_to_offset(1.0, 400.0, 564.0) == 0.0,
    );

    // -- the storage report must say plainly that nothing is charged
    {
        use library::{BulkInfo, MediaItem, ScanRow};
        let row = ScanRow {
            item: MediaItem {
                media_key: "K".into(),
                dedup_key: "D".into(),
                thumb: String::new(),
                title: "a clip".into(),
                w: 1920,
                h: 1080,
                is_favorite: false,
                ts: 1_500_000_000,
                is_video: true,
                duration_ms: 31_000,
            },
            info: BulkInfo {
                source: 0,
                media_key: "K".into(),
                filename: "a clip.mp4".into(),
                description: String::new(),
                size: 286_261_248,
                timestamp: 1_500_000_000,
                tz_offset: 0,
                takes_up_space: Some(false),
                space_flag: Some(2),
                orig_flag: None,
                original_quality: None,
            },
            list: "timeline",
        };
        let st = library::storage_stats(&[row]);
        let lines = library::stats_lines(&st);
        check(
            "report says nothing is charged",
            lines
                .iter()
                .any(|(k, v)| k == "counted toward your storage" && v.contains("0 of them take space")),
        );
        check(
            "report prints the size and the bitrate",
            lines.iter().any(|(_, v)| v.contains("273.0 MB"))
                && lines.iter().any(|(_, v)| v.contains("Mbps")),
        );
    }

    // -- resume rule
    check("resume: mid film", App::resume_allowed(300.0, 1800.0));
    check("resume: near the end is finished", !App::resume_allowed(1795.0, 1800.0));
    check("resume: a short clip is never resumed", !App::resume_allowed(20.0, 30.0));

    // -- jump-to-date parsing and the exported Colab notebook
    {
        let jul = library::days_from_civil(2019, 7, 1) * 86400;
        let day = library::days_from_civil(2019, 7, 15) * 86400;
        check("jump date: jul 2019", parse_date_query("jul 2019") == Some(jul));
        check("jump date: 2019-07", parse_date_query("2019-07") == Some(jul));
        check("jump date: 15 jul 2019", parse_date_query("15 jul 2019") == Some(day));
        check("jump date: 2019-07-15", parse_date_query("2019-07-15") == Some(day));
        check("jump date: bare year", parse_date_query("2019")
            == Some(library::days_from_civil(2019, 1, 1) * 86400));
        check("jump date: nonsense refused", parse_date_query("banana").is_none());
        let nb = colab_notebook(2);
        check(
            "colab notebook is valid json",
            serde_json::from_str::<serde_json::Value>(&nb).is_ok(),
        );
        let scratch = std::env::temp_dir().join("opencode").join("_selftest_job");
        let _ = std::fs::remove_dir_all(&scratch);
        let row = StorageRow {
            key: "KEY1".into(),
            dedup: "DEDUP1".into(),
            thumb: "https://lh3.googleusercontent.com/p/abc=w400".into(),
            ts: 1556949153,
            size: 42973,
            filename: "one.jpg".into(),
        };
        let wrote = match App::write_colab_job_at(scratch.clone(), &[row]) {
            Some(d) => {
                let tasks = std::fs::read_to_string(d.join("tasks.tsv")).unwrap_or_default();
                let lines = tasks.lines().count();
                let no_signed = !tasks.contains("=d");
                let urls_ok = std::fs::read_to_string(d.join("urls.tsv"))
                    .map(|u| u.contains("=d"))
                    .unwrap_or(false);
                let nb_ok = d.join("gphotos_free.ipynb").exists();
                lines == 2 && no_signed && urls_ok && nb_ok
            }
            None => false,
        };
        check("colab job writes manifest + notebook", wrote);
    }

    // -- quota + links (storage line and the sharing tab)
    check("storage quota reads", library::quota(&s).is_some());
    let (al, _) = library::albums(&s, None);
    check("albums list reads", !al.is_empty());
    let (lk, _) = library::links(&s, None);
    println!("note     {} shared links listed", lk.len());

    println!("\n{} checks, {} failed", total, fails);
    if fails == 0 {
        0
    } else {
        1
    }
}

fn today_start() -> i64 {
    // midnight local time, as a unix timestamp
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = now / 86400;
    let (y, m, d) = library::civil_from_days(days);
    let (_, _, _) = (y, m, d);
    now - (now % 86400)
}

fn now_stamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

// A launch link targets one photo or one album. The optional account is the
// email stored in the link by whoever created it, so the right profile can be
// selected before the target is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Photo(Option<String>),
    Album(Option<String>),
}

impl Kind {
    fn account(&self) -> Option<&str> {
        match self {
            Kind::Photo(a) | Kind::Album(a) => a.as_deref(),
        }
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// Percent-decode without a dependency; keys and account emails are short ASCII.
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if i + 2 < b.len() && b[i] == b'%' {
            if let (Some(hi), Some(lo)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn deeplink_target(path: &str) -> Option<(Kind, String)> {
    let (build, rest) = if let Some(r) = path.strip_prefix("photo/") {
        (Kind::Photo as fn(Option<String>) -> Kind, r)
    } else if let Some(r) = path.strip_prefix("album/") {
        (Kind::Album as fn(Option<String>) -> Kind, r)
    } else {
        return None;
    };
    let (target, query) = rest.split_once('?').unwrap_or((rest, ""));
    let key = url_decode(target.split(|c| c == '/' || c == '#').next().unwrap_or(""));
    if key.is_empty() {
        return None;
    }
    let account = query.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        if k == "account" || k == "email" {
            let v = url_decode(v);
            if !v.is_empty() {
                return Some(v);
            }
        }
        None
    });
    Some((build(account), key))
}

// Pure parser: see the deeplink_tests module for every accepted form.
fn parse_deeplink(input: &str) -> Option<(Kind, String)> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if s.to_lowercase().ends_with(".gphotoslink") {
        let text = std::fs::read_to_string(s).ok()?;
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))?;
        return parse_deeplink(line);
    }
    if let Some(rest) = s.strip_prefix("gphotos://") {
        return deeplink_target(rest);
    }
    // a web share URL carries the key in its path; short goo.gl links cannot
    // be resolved offline, so they fall through to None (honest error toast)
    if let Some(rest) = s
        .strip_prefix("https://photos.google.com/")
        .or_else(|| s.strip_prefix("http://photos.google.com/"))
    {
        return deeplink_target(rest);
    }
    None
}



fn main() -> eframe::Result<()> {
    // --info <media key>… prints what Google says about an item: its real
    // timestamp, size, whether it takes quota space, and its quality.
    let argv: Vec<String> = std::env::args().collect();
    // --import-cookies <file>: headless Windows-ABE import. Prints only a count
    // on success; a rejected file never exposes its contents in output/logs.
    if let Some(p) = argv.iter().position(|a| a == "--import-cookies") {
        let result = argv
            .get(p + 1)
            .filter(|a| !a.starts_with('-'))
            .map(|path| import_cookie_file(std::path::Path::new(path)))
            .unwrap_or(Err(()));
        match result {
            Ok(n) => println!("imported {n} account(s) from the local cookie export"),
            Err(()) => {
                eprintln!("cookie export rejected: file missing, unreadable, or invalid format");
                std::process::exit(1);
            }
        }
        std::process::exit(0);
    }
    if let Some(p) = argv.iter().position(|a| a == "--info") {
        let keys: Vec<String> = argv[p + 1..].to_vec();
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        if std::env::var("GPHOTOS_RAW").is_ok() {
            if let Some(v) = library::bulk_raw(&s, &keys) {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            std::process::exit(0);
        }
        for b in library::bulk_info(&s, &keys) {
            println!(
                "{}  ts={} ({})  size={}  takes_space={}  original={}",
                b.media_key,
                b.timestamp,
                date_label(b.timestamp),
                b.size,
                b.takes_up_space == Some(true),
                b.original_quality == Some(true)
            );
        }
        std::process::exit(0);
    }
    // --storage-report: every number the storage page shows, in the terminal
    if argv.iter().any(|a| a == "--storage-report") {
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        eprintln!("reading every list and asking Google about each item…");
        let rows = library::scan_all(&s);
        let st = library::storage_stats(&rows);
        for (k, v) in library::stats_lines(&st) {
            println!("{:<28} {}", k, v);
        }
        if let Some(p) = argv.iter().position(|a| a == "--storage-report") {
            if let Some(out) = argv.get(p + 1).filter(|a| !a.starts_with('-')) {
                let mut tsv = String::from(
                    "media_key\tts\tsize\tkind\tspace_flag\torig_flag\tsource\tlist\tfilename\n",
                );
                for r in &rows {
                    tsv.push_str(&format!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                        r.item.media_key,
                        r.info.timestamp / 1000,
                        r.info.size,
                        if r.item.is_video { "video" } else { "photo" },
                        r.info.space_flag.map(|v| v.to_string()).unwrap_or_default(),
                        r.info.orig_flag.map(|v| v.to_string()).unwrap_or_default(),
                        r.info.source,
                        r.list,
                        r.info.filename.replace('\t', " ")
                    ));
                }
                if let Err(e) = write_secret_file(std::path::Path::new(out), &tsv) {
                    eprintln!("could not write {}: {}", out, e);
                }
                println!("\nrow per item written to {}", out);
            }
        }
        std::process::exit(0);
    }
    // --albums: list album keys and titles (for housekeeping)
    if argv.iter().any(|a| a == "--albums") {
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        let (al, _) = library::albums(&s, None);
        for a in al {
            println!("{}\t{}\t{} items\t{}", a.key, a.title, a.count, if a.is_shared { "shared" } else { "" });
        }
        std::process::exit(0);
    }
    // --create-album <name>: make an empty album
    if let Some(p) = argv.iter().position(|a| a == "--create-album") {
        let name = argv.get(p + 1).cloned().unwrap_or_else(|| "new album".into());
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        match library::create_album(&s, &name) {
            Some(k) => println!("{}	{}", k, name),
            None => println!("could not create album {}", name),
        }
        std::process::exit(0);
    }
    // --delete-album <album key…>: removes albums, never their photos
    if let Some(p) = argv.iter().position(|a| a == "--delete-album") {
        let keys: Vec<String> = argv[p + 1..].to_vec();
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        for k in keys {
            println!("{} delete album: {}", k, if library::delete_album(&s, &k) { "ok" } else { "failed" });
        }
        std::process::exit(0);
    }
    // --share <media key…>: make a photos.app.goo.gl link and copy it locally
    if let Some(p) = argv.iter().position(|a| a == "--share") {
        let keys: Vec<String> = argv[p + 1..].to_vec();
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        eprintln!("asking for a link…");
        match library::create_share_link(&s, &keys) {
            Some(url) => {
                eprintln!("got link");
                println!("{}", url);
                eprintln!("copying…");
                println!(
                    "clipboard: {}",
                    if copy_text_to_clipboard(&url) { "ok" } else { "unavailable" }
                );
            }
            None => println!("could not create a sharing link"),
        }
        std::process::exit(0);
    }
    // --quota-audit <out.tsv>: the same walk, written out as a table
    if let Some(p) = argv.iter().position(|a| a == "--quota-audit") {
        let out_path = argv.get(p + 1).cloned().unwrap_or_else(|| "quota.tsv".into());
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        let rows = library::scan_all(&s);
        eprintln!("  {} unique items measured", rows.len());
        let mut table: Vec<(u64, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.info.size,
                    format!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        r.item.media_key,
                        r.info.timestamp / 1000,
                        r.info.size,
                        if r.item.is_video { "video" } else { "photo" },
                        r.info.space_flag.map(|v| v.to_string()).unwrap_or_default(),
                        r.info.orig_flag.map(|v| v.to_string()).unwrap_or_default(),
                        r.info.source,
                        r.list,
                        r.info.filename.replace('\t', " ")
                    ),
                )
            })
            .collect();
        table.sort_by(|a, b| b.0.cmp(&a.0));
        let mut out = String::from(
            "media_key\tts\tsize\tkind\tspace_flag\torig_flag\tsource\tlist\tfilename\n",
        );
        for (_, line) in &table {
            out.push_str(line);
            out.push('\n');
        }
        if let Err(e) = write_secret_file(std::path::Path::new(&out_path), &out) {
            eprintln!("could not write {}: {}", out_path, e);
        }
        let total: u64 = rows.iter().map(|r| r.info.size).sum();
        println!(
            "{} items, {} of media, written to {}",
            rows.len(),
            human_gb(total),
            out_path
        );
        let st = library::storage_stats(&rows);
        for (src, (n, sz)) in st
            .space_flags
            .iter()
            .map(|(k, n, b)| (k.clone(), (*n, *b)))
            .collect::<Vec<_>>()
        {
            println!("  space flag {}: {} items, {}", src, n, human_gb(sz));
        }
        std::process::exit(0);
    }
    // --dedup <media key>: the timeline row carries the id the action RPCs want
    if let Some(p) = argv.iter().position(|a| a == "--dedup") {
        let want: Vec<String> = argv[p + 1..].to_vec();
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        type Walk = fn(&library::Session, Option<&str>) -> (Vec<library::MediaItem>, Option<String>);
        let sources: [(&str, Walk); 5] = [
            ("timeline", library::recent),
            ("archive", library::archive),
            ("locked", library::locked),
            ("favourites", library::favorites),
            ("trash", library::trash),
        ];
        let mut found = 0;
        for (name, f) in sources {
            let mut page: Option<String> = None;
            for _ in 0..500 {
                let (items, next) = f(&s, page.as_deref());
                if items.is_empty() {
                    break;
                }
                for it in items {
                    if want.iter().any(|w| *w == it.media_key) {
                        println!(
                            "{}\t{}\t{}\t{}\t{}",
                            it.media_key, it.dedup_key, name, it.ts, it.title
                        );
                        found += 1;
                    }
                }
                page = match next {
                    Some(t) => Some(t),
                    None => break,
                };
            }
        }
        if found == 0 {
            println!("not found in any list");
        }
        std::process::exit(0);
    }
    if let Some(p) = argv.iter().position(|a| a == "--find") {
        let q = argv.get(p + 1).cloned().unwrap_or_default();
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        let (items, _) = library::search(&s, &q, None);
        for it in items {
            println!(
                "{}\t{}\t{}\t{}",
                it.media_key, it.dedup_key, it.ts, it.title
            );
        }
        std::process::exit(0);
    }
    if let Some(p) = argv.iter().position(|a| a == "--purge") {
        let keys: Vec<String> = argv[p + 1..].to_vec();
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        let ok = library::delete_forever(&s, &keys);
        println!("{} purged: {}", if ok { "ok" } else { "failed" }, keys.join(", "));
        std::process::exit(0);
    }
    if let Some(p) = argv.iter().position(|a| a == "--trash") {
        let keys: Vec<String> = argv[p + 1..].to_vec();
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        let ok = library::set_trashed(&s, &keys, true);
        println!("{} trashed: {}", if ok { "ok" } else { "failed" }, keys.join(", "));
        std::process::exit(0);
    }
    // --colab-job: headless version of the settings button, for a machine with
    // no window (or for running the same thing from a script)
    if argv.iter().any(|a| a == "--colab-job") {
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        type Walk = fn(&library::Session, Option<&str>) -> (Vec<library::MediaItem>, Option<String>);
        let sources: [(&str, Walk); 5] = [
            ("timeline", library::recent),
            ("archive", library::archive),
            ("locked", library::locked),
            ("favourites", library::favorites),
            ("trash", library::trash),
        ];
        let mut all: Vec<library::MediaItem> = vec![];
        for (_, f) in sources {
            let mut page: Option<String> = None;
            for _ in 0..600 {
                let (mut got, next) = f(&s, page.as_deref());
                let n = got.len();
                all.append(&mut got);
                page = match next {
                    Some(t) if n > 0 => Some(t),
                    _ => None,
                };
                if page.is_none() {
                    break;
                }
            }
        }
        all.sort_by(|a, b| a.media_key.cmp(&b.media_key));
        all.dedup_by(|a, b| a.media_key == b.media_key);
        eprintln!("{} items, asking which take storage…", all.len());
        let mut rows: Vec<StorageRow> = vec![];
        for group in all.chunks(105) {
            let keys: Vec<String> = group.iter().map(|i| i.media_key.clone()).collect();
            for info in library::bulk_info(&s, &keys) {
                if info.takes_up_space != Some(true) {
                    continue;
                }
                let Some(it) = group.iter().find(|i| i.media_key == info.media_key) else {
                    continue;
                };
                rows.push(StorageRow {
                    key: info.media_key.clone(),
                    dedup: it.dedup_key.clone(),
                    thumb: it.thumb.clone(),
                    ts: info.timestamp / 1000,
                    size: info.size,
                    filename: info.filename.clone(),
                });
            }
        }
        match App::write_colab_job(&rows) {
            Some(dir) => println!(
                "{} item(s) → {}\nupload tasks.tsv + gphotos_free.ipynb + gotohp-cli to Drive (keep urls.tsv local)",
                rows.len(),
                dir.display()
            ),
            None => eprintln!("could not write the job folder"),
        }
        std::process::exit(0);
    }
    if argv.iter().any(|a| a == "--quota") {
        let Some(s) = library::Session::capture() else {
            eprintln!("no session");
            std::process::exit(1);
        };
        if std::env::var("GPHOTOS_RAW").is_ok() {
            if let Some(v) = s.rpc("EzwWhf", &serde_json::json!([])) {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            std::process::exit(0);
        }
        match library::quota(&s) {
            Some(q) => println!(
                "account used={} available={} photos={} (raw bytes? {})",
                human_gb(q.used),
                human_gb(q.available),
                human_gb(q.gphotos),
                q.gphotos
            ),
            None => println!("quota unreadable"),
        }
        std::process::exit(0);
    }
    if std::env::args().any(|a| a == "--selftest") {
        let pure_fail = selftest_pure();
        let live_fail = selftest();
        std::process::exit(if pure_fail == 0 && live_fail == 0 { 0 } else { 1 });
    }
    // A deeplink is just another argv: a gphotos:// URL or a .gphotoslink path.
    // Parsing here keeps the GUI free of argv handling; the bad-link toast is
    // carried into App so it shows once the window is up.
    let link_arg = argv.iter().skip(1).find(|a| {
        a.starts_with("gphotos://") || a.to_lowercase().ends_with(".gphotoslink")
    }).cloned();
    let deeplink = link_arg.as_deref().and_then(parse_deeplink);
    let bad_link = link_arg.is_some() && deeplink.is_none();
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 760.0])
            .with_title("Google Photos")
            .with_icon(window_icon().unwrap_or(egui::IconData::default())),
        ..Default::default()
    };
    eframe::run_native(
        "Google Photos",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(cc, deeplink, bad_link)))),
    )
}

// A/V clock byte accounting: the sink's callback and this test share the exact
// same helper, so the atomic never drifts from the PCM actually handed to the
// device.
#[cfg(test)]
mod audio_tests {
    use super::*;

    #[test]
    fn byte_accounting_counts_exact_consumed_bytes() {
        let clock = AtomicU64::new(0);
        let pcm = vec![0xA5u8; 1000];
        let mut device = vec![0u8; 4096];

        // fake sink consumes all the available PCM
        let n = consume_pcm(&mut device, &pcm, &clock);
        assert_eq!(n, 1000);
        assert_eq!(clock.load(Ordering::Relaxed), 1000);
        assert!(device[..1000].iter().all(|&b| b == 0xA5));

        // a smaller device buffer consumes only what fits — no over-counting
        let mut small = vec![0u8; 256];
        let n2 = consume_pcm(&mut small, &pcm, &clock);
        assert_eq!(n2, 256);
        assert_eq!(clock.load(Ordering::Relaxed), 1256);

        // underrun: no PCM available, so the clock must not advance at all
        let n3 = consume_pcm(&mut device, &[], &clock);
        assert_eq!(n3, 0);
        assert_eq!(clock.load(Ordering::Relaxed), 1256);
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;

    #[test]
    fn export_filename_sanitizes_and_deduplicates() {
        let mut used = std::collections::HashSet::new();
        let a = format!("holiday_photo 1_{:06x}.JPG", crc32(b"keyA") & 0xfff_fff);
        assert_eq!(export_filename("holiday/photo 1.JPG", "keyA", false, &mut used), a);
        assert_eq!(export_filename("holiday\\photo 1.JPG", "keyA", false, &mut used), format!("holiday_photo 1_{:06x}_2.JPG", crc32(b"keyA") & 0xfff_fff));
        assert_eq!(export_filename("clip", "keyB", true, &mut used), format!("clip_{:06x}.mp4", crc32(b"keyB") & 0xfff_fff));
        assert_eq!(export_filename("..", "keyC", false, &mut used), format!("_{:06x}.jpg", crc32(b"keyC") & 0xfff_fff));
        assert_eq!(export_filename("", "keyD", true, &mut used), format!("_{:06x}.mp4", crc32(b"keyD") & 0xfff_fff));
    }

    #[test]
    fn local_filter_names_and_predicates() {
        assert_eq!(filter_view_name(FILTER_VIDEOS), "videos");
        assert_eq!(filter_view_name(FILTER_PHOTOS), "photos");
        assert_eq!(filter_view_name(FILTER_LARGE), "large");
        assert_eq!(filter_view_name(FILTER_VIDEOS | FILTER_LARGE), "filter:videos+large");
        assert_eq!(filter_view_name(FILTER_PHOTOS | FILTER_LARGE), "filter:photos+large");

        assert!(filter_matches(FILTER_VIDEOS, true, 0));
        assert!(!filter_matches(FILTER_VIDEOS, false, u64::MAX));
        assert!(filter_matches(FILTER_PHOTOS, false, 0));
        assert!(!filter_matches(FILTER_PHOTOS, true, 0));
        assert!(filter_matches(FILTER_LARGE, true, LARGE_FILE_BYTES));
        assert!(!filter_matches(FILTER_LARGE, true, LARGE_FILE_BYTES - 1));
        // the combination the old replace-the-view chips could not express
        let both = FILTER_VIDEOS | FILTER_LARGE;
        assert!(filter_matches(both, true, LARGE_FILE_BYTES));
        assert!(!filter_matches(both, true, LARGE_FILE_BYTES - 1));
        assert!(!filter_matches(both, false, LARGE_FILE_BYTES));
    }

    #[test]
    fn registry_merge_preserves_owner_fields_and_stages_new_accounts() {
        use library::AccountRecord;
        let known = AccountRecord {
            jar: "brave|Default".into(),
            email: "one@gmail.com".into(),
            name: "One".into(),
            hidden: true,
            last_seen: 100,
            last_ok: 90,
        };
        // discovery reports the same identity with its defaults
        let discovered_known = AccountRecord {
            jar: known.jar.clone(),
            email: known.email.clone(),
            name: "Renamed".into(),
            hidden: false,
            last_seen: 999,
            last_ok: 0,
        };
        let brand_new = AccountRecord {
            jar: "brave|Profile 1".into(),
            email: "two@gmail.com".into(),
            name: "Two".into(),
            hidden: false,
            last_seen: 999,
            last_ok: 0,
        };
        let (persist, staged) =
            merge_accounts(&[known.clone()], &[discovered_known, brand_new.clone()]);
        // already-known identity keeps the owner's hidden/last_seen/last_ok verbatim
        assert_eq!(persist, vec![known.clone()]);
        // a brand-new identity is staged, never written to the registry
        assert!(staged.contains(&brand_new));
        assert!(!persist.iter().any(|a| same_account(a, &brand_new)));
    }
}

#[cfg(test)]
mod accounts_tests {
    use super::*;
    use library::AccountRecord;

    #[test]
    fn secret_files_are_owner_only() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = std::env::temp_dir().join(format!("gphotos-mode-{}", std::process::id()));
            let _ = std::fs::remove_file(&p);
            write_secret_file(&p, "selected_email=x@y.z\n").unwrap();
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "fresh secret file must be 0600");

            // both set_setting and write_hidden now go through write_secret_file,
            // so even a legacy world-readable file gets tightened on rewrite
            std::fs::write(&p, "old\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            write_secret_file(&p, "selected_email=x@y.z\n").unwrap();
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "existing secret file must be tightened to 0600");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn stale_session_cannot_serve_a_switched_account() {
        let old = library::Session {
            cookie: String::new(), at: String::new(), fsid: String::new(),
            bl: String::new(), path: "/".into(), rapt: String::new(),
            fife_cookie: String::new(),
            jar: "chrome|Profile 1|Network/Cookies".into(),
            email: "old@gmail.com".into(),
        };
        let sel = Some(("chrome|Profile 1|Network/Cookies".into(), "new@gmail.com".into()));
        assert!(!session_matches(&sel, &old), "old email must be rejected");
        assert!(session_matches(
            &Some((old.jar.clone(), old.email.clone())), &old
        ));
        // a session from a different profile is not valid either
        assert!(!session_matches(
            &Some(("chrome|Profile 2|Network/Cookies".into(), "old@gmail.com".into())),
            &old
        ));
    }

    #[test]
    fn add_account_opens_the_discovered_profile() {
        // gcookies.py emits "browser|profile|cookie-db-path"; the profile is
        // field 1 and may be nested — only its last component is a Chrome dir.
        assert_eq!(
            jar_browser_argv("chrome|Profile 1|Network/Cookies", "https://signin"),
            Some(("google-chrome".into(), vec![
                "--profile-directory=Profile 1".into(), "https://signin".into()
            ]))
        );
        assert_eq!(
            jar_browser_argv("brave|Brave-Origin/Default|Cookies", "https://signin"),
            Some(("brave-browser".into(), vec![
                "--profile-directory=Default".into(), "https://signin".into()
            ]))
        );
        // Firefox selects its profile with -P (no --profile-directory)
        assert_eq!(
            jar_browser_argv("firefox|abc.default|cookies.sqlite", "https://signin"),
            Some(("firefox".into(), vec!["-P".into(), "abc.default".into(), "https://signin".into()]))
        );
        // non-filesystem labels fall back to the OS URL helper
        assert_eq!(jar_browser_argv("imported|Default", "https://signin"), None);
        assert_eq!(jar_browser_argv("unknown|Default|x", "https://signin"), None);
    }

    fn rec(jar: &str, email: &str) -> AccountRecord {
        AccountRecord {
            jar: jar.into(),
            email: email.into(),
            name: String::new(),
            hidden: false,
            last_seen: 0,
            last_ok: 0,
        }
    }

    #[test]
    fn hidden_keys_are_per_profile_and_legacy_file_migrates() {
        let rows = vec![
            rec("brave|Default", "same@gmail.com"),
            rec("brave|Profile 1", "same@gmail.com"),
        ];
        // old email-only line plus one already-migrated line
        let text = "same@gmail.com\nchrome|Other\tzoe@gmail.com\n";
        let (mut hidden, migrated) = parse_hidden(text, &rows);
        assert!(migrated);
        assert!(hidden.contains(&("brave|Default".into(), "same@gmail.com".into())));
        assert!(hidden.contains(&("brave|Profile 1".into(), "same@gmail.com".into())));
        assert!(hidden.contains(&("chrome|Other".into(), "zoe@gmail.com".into())));
        assert_eq!(hidden.len(), 3);
        assert!(!hidden.contains(&(String::new(), "same@gmail.com".into())));

        // hiding one profile must not hide the other profile with the same email
        hidden.remove(&("brave|Default".into(), "same@gmail.com".into()));
        assert!(!hidden.contains(&("brave|Default".into(), "same@gmail.com".into())));
        assert!(hidden.contains(&("brave|Profile 1".into(), "same@gmail.com".into())));
    }

    #[test]
    fn legacy_hidden_email_without_registry_row_keeps_all_profile_sentinel() {
        // A real legacy upgrade has an empty registry: never drop the choice.
        let (keys, migrated) = parse_hidden("lost@gmail.com\n", &[]);
        assert!(migrated);
        assert!(keys.contains(&(String::new(), "lost@gmail.com".into())));
        assert!(is_hidden(&keys, "brave|Default", "lost@gmail.com"));
        assert!(is_hidden(&keys, "chrome|Profile 1", "lost@gmail.com"));
        assert!(!is_hidden(&keys, "brave|Default", "other@gmail.com"));
        // a rewrite must not destroy the sentinel: bare email round-trips
        let lines = hidden_lines(&keys);
        assert_eq!(lines, vec!["lost@gmail.com".to_string()]);
        let (again, _) = parse_hidden(&lines.join("\n"), &[]);
        assert_eq!(again, keys);
    }

    #[test]
    fn explicit_add_inserts_staged_identity_once() {
        let known = rec("brave|Default", "one@gmail.com");
        let staged = rec("brave|Profile 1", "two@gmail.com");
        let rows = confirm_account(&[known.clone()], &staged);
        assert_eq!(rows.len(), 2);
        let added = rows.iter().find(|r| same_account(r, &staged)).unwrap();
        assert!(!added.hidden);
        // re-confirming the same identity is a no-op
        assert_eq!(confirm_account(&rows, &staged), rows);
    }

    #[test]
    fn selecting_a_staged_identity_leaves_registry_unchanged() {
        let known = rec("brave|Default", "one@gmail.com");
        let staged = rec("brave|Profile 1", "two@gmail.com");
        let discovered = vec![known.clone(), staged.clone()];
        let (confirmed, staged_rows) = merge_accounts(&[known.clone()], &discovered);
        assert_eq!(confirmed, vec![known.clone()]);
        assert!(staged_rows.contains(&staged));

        // "use" writes settings only: the registry stays at `confirmed`, so the
        // chosen identity is still staged on the next rescan.
        let registry_after_use = confirmed;
        let (_, still_staged) = merge_accounts(&registry_after_use, &discovered);
        assert!(still_staged.contains(&staged));

        // only an explicit Add promotion removes it from the staged set
        let after_add = confirm_account(&registry_after_use, &staged);
        let (_, staged_after_add) = merge_accounts(&after_add, &discovered);
        assert!(staged_after_add.is_empty());
    }

    #[test]
    fn cookie_export_parser_accepts_valid_file() {
        let text = r#"{"version":1,"accounts":[
            {"jar":"imported|Default","email":"you@gmail.com",
             "cookie":"SID=abc; HSID=def","fife_cookie":"OSID=xyz"}
        ]}"#;
        let rows = parse_cookie_export(text).expect("valid export must parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].jar, "imported|Default");
        assert_eq!(rows[0].email, "you@gmail.com");
        assert_eq!(rows[0].cookie, "SID=abc; HSID=def");
        assert_eq!(rows[0].fife_cookie, "OSID=xyz");
        // fife_cookie and version are optional
        let minimal = r#"{"accounts":[{"jar":"j","email":"a@b","cookie":"SID=x"}]}"#;
        assert_eq!(parse_cookie_export(minimal).unwrap().len(), 1);
    }

    #[test]
    fn cookie_export_parser_rejects_malformed_and_unsafe_input() {
        let bad = [
            "not json at all",
            "[]",
            "{}",
            r#"{"accounts":[]}"#,
            r#"{"version":2,"accounts":[{"jar":"j","email":"a@b","cookie":"SID=x"}]}"#,
            // wrong field types
            r#"{"accounts":[{"jar":1,"email":"a@b","cookie":"SID=x"}]}"#,
            r#"{"accounts":"not-an-array"}"#,
            // missing / empty required fields
            r#"{"accounts":[{"jar":"j","email":"a@b"}]}"#,
            r#"{"accounts":[{"jar":"","email":"a@b","cookie":"SID=x"}]}"#,
            r#"{"accounts":[{"jar":"j","email":"","cookie":"SID=x"}]}"#,
            r#"{"accounts":[{"jar":"j","email":"a@b","cookie":""}]}"#,
            // email sanity and cookie header injection
            r#"{"accounts":[{"jar":"j","email":"not-an-email","cookie":"SID=x"}]}"#,
            r#"{"accounts":[{"jar":"j","email":"a@b","cookie":"no-equals"}]}"#,
            r#"{"accounts":[{"jar":"j","email":"a@b","cookie":"SID=x\r\nSet-Cookie: evil=y"}]}"#,
            r#"{"accounts":[{"jar":"j\nk","email":"a@b","cookie":"SID=x"}]}"#,
            r#"{"accounts":[{"jar":"j","email":"a@b","cookie":"SID=x","fife_cookie":"a\nb"}]}"#,
        ];
        for text in &bad {
            assert!(parse_cookie_export(text).is_err(), "must reject: {text}");
        }
        // oversized input is rejected before parsing
        let big = format!(r#"{{"accounts":[{},{{"jar":"j","email":"a@b","cookie":"SID=x"}}]}}"#, "x".repeat(MAX_COOKIE_EXPORT_BYTES));
        assert!(parse_cookie_export(&big).is_err());
    }
}

#[cfg(test)]
mod portability_tests {
    use super::*;

    #[test]
    fn app_dir_override_beats_os_base() {
        let base = PathBuf::from("/os/base");
        assert_eq!(
            app_dir(Some("/custom/root"), Some(base)),
            PathBuf::from("/custom/root")
        );
    }

    #[test]
    fn app_dir_falls_back_to_base_then_temp() {
        assert_eq!(
            app_dir(None, Some(PathBuf::from("/os/base"))),
            PathBuf::from("/os/base/gphotos-app")
        );
        assert_eq!(
            app_dir(None, None),
            std::env::temp_dir().join("gphotos-app")
        );
    }

    #[test]
    fn systemd_unit_path_is_not_app_scoped() {
        let p = systemd_unit_path(PathBuf::from("/config/base"));
        assert_eq!(
            p,
            PathBuf::from("/config/base").join("systemd").join("user").join(UNIT)
        );
        assert!(!p.components().any(|c| c.as_os_str() == "gphotos-app"));
        assert!(!config_base().components().any(|c| c.as_os_str() == "gphotos-app"));
    }

    #[test]
    fn url_open_command_targets_the_os_helper() {
        let (prog, args) = url_open_command("https://example.com/a?b=1");
        assert!(args.iter().any(|a| a == "https://example.com/a?b=1"));
        #[cfg(target_os = "linux")]
        assert_eq!(prog, "xdg-open");
        #[cfg(target_os = "windows")]
        assert_eq!((prog, args.len()), ("cmd", 4));
        #[cfg(target_os = "macos")]
        assert_eq!(prog, "open");
    }

    #[test]
    fn dialog_picks_map_to_nonempty_strings() {
        assert_eq!(
            picked_folder(Some(PathBuf::from("/media/photos"))).as_deref(),
            Some("/media/photos")
        );
        assert_eq!(picked_folder(None), None);
        assert_eq!(picked_folder(Some(PathBuf::new())), None);
        assert_eq!(
            picked_files(Some(vec![
                PathBuf::from("a.jpg"),
                PathBuf::new(),
                PathBuf::from("b.jpg"),
            ])),
            vec!["a.jpg", "b.jpg"]
        );
        assert!(picked_files(None).is_empty());
    }

    #[test]
    fn null_device_is_platform_specific() {
        assert_eq!(null_device(false), "/dev/null");
        assert_eq!(null_device(true), "NUL");
    }

    #[test]
    fn clipboard_owner_can_be_shared_for_the_process_lifetime() {
        fn assert_static_send_sync<T: Send + Sync + 'static>() {}
        assert_static_send_sync::<arboard::Clipboard>();
    }

    #[test]
    fn timestamp_maps_unix_seconds_to_system_time() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = timestamp_from_unix(1_600_000_000).unwrap();
        assert_eq!(t, UNIX_EPOCH + Duration::from_secs(1_600_000_000));
        assert_eq!(timestamp_from_unix(0), Some(UNIX_EPOCH));
        assert_eq!(
            timestamp_from_unix(-1),
            Some(UNIX_EPOCH - Duration::from_secs(1))
        );
    }

    #[test]
    fn linux_sync_availability_maps_only_linux() {
        assert!(linux_sync_available_on("linux"));
        assert!(!linux_sync_available_on("windows"));
        assert!(!linux_sync_available_on("macos"));
        assert!(!linux_sync_available_on("darwin"));
        assert!(!linux_sync_available_on("freebsd"));
        assert_eq!(linux_sync_available(), cfg!(target_os = "linux"));
    }

    #[test]
    fn linux_sync_disabled_notice_is_honest() {
        assert_eq!(linux_sync_notice(true), "");
        let off = linux_sync_notice(false);
        assert!(!off.is_empty());
        let low = off.to_lowercase();
        assert!(low.contains("linux"), "must name the OS the feature needs");
        assert!(low.contains("not available"), "must say it is unavailable");
        assert!(!low.contains("partial"), "must not promise partial support");
        assert!(!low.contains("workaround"), "must not promise a substitute");
        assert!(!low.contains("will work"), "must not promise it works here");
    }

    #[test]
    fn local_uploader_disabled_notice_is_honest() {
        assert_eq!(local_uploader_notice(true), "");
        let off = local_uploader_notice(false);
        assert!(!off.is_empty());
        let low = off.to_lowercase();
        assert!(low.contains("linux"), "must name the OS the uploader needs");
        assert!(low.contains("not available"), "must say it is unavailable");
        assert!(low.contains("colab"), "must point to the working cross-platform path");
        assert!(low.contains("works here"), "must say the rest of the page still works");
        assert!(!low.contains("partial"), "must not promise partial support");
    }

    #[test]
    fn local_uploader_controls_disabled_off_linux() {
        // The disabled state itself, not just its wording: every control that
        // spawns gotohp (upload button, make-free buttons, local engine radio)
        // follows this one mapping, so a non-Linux build has no live dead button.
        assert!(local_uploader_control_enabled_on("linux"));
        for os in ["windows", "macos", "darwin", "freebsd"] {
            assert!(!local_uploader_control_enabled_on(os), "{os} must disable uploader controls");
        }
        assert_eq!(local_uploader_control_enabled(), cfg!(target_os = "linux"));
        // Notice and disabled state must agree: shown exactly when disabled.
        for os in ["linux", "windows", "macos"] {
            let on = local_uploader_control_enabled_on(os);
            assert_eq!(local_uploader_notice(on).is_empty(), on);
        }
    }
}

#[cfg(test)]
mod deeplink_tests {
    use super::*;

    // a .gphotoslink is just a text file; no network, no RPC, no secrets
    fn link_file(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "gphotos-link-{}-{}.gphotoslink",
            std::process::id(),
            name
        ));
        std::fs::write(&p, body).expect("write temp .gphotoslink");
        p
    }

    #[test]
    fn photo_url_yields_photo_key() {
        assert_eq!(
            parse_deeplink("gphotos://photo/AF1QipABC123"),
            Some((Kind::Photo(None), "AF1QipABC123".into()))
        );
    }

    #[test]
    fn album_url_yields_album_key() {
        assert_eq!(
            parse_deeplink("gphotos://album/AAK-1234567890"),
            Some((Kind::Album(None), "AAK-1234567890".into()))
        );
    }

    #[test]
    fn link_file_with_gphotos_url_opens_it() {
        let p = link_file("gphotos", "# shared from the desktop app\ngphotos://photo/FromFileKey\n");
        let got = parse_deeplink(&p.to_string_lossy());
        let _ = std::fs::remove_file(&p);
        assert_eq!(got, Some((Kind::Photo(None), "FromFileKey".into())));
    }

    #[test]
    fn link_file_with_share_url_opens_it() {
        let p = link_file("share", "https://photos.google.com/photo/SharePhotoKey?key=ignored\n");
        let got = parse_deeplink(&p.to_string_lossy());
        let _ = std::fs::remove_file(&p);
        assert_eq!(got, Some((Kind::Photo(None), "SharePhotoKey".into())));
    }

    #[test]
    fn malformed_and_unknown_schemes_are_rejected() {
        for bad in [
            "mailto:someone@example.com",
            "gphotos://video/AF1Qip",
            "gphotos://photo/",
            "gphotos://album/",
            "https://photos.app.goo.gl/opaqueToken",
            "   ",
        ] {
            assert_eq!(parse_deeplink(bad), None, "must reject {bad}");
        }
    }

    #[test]
    fn keys_and_account_are_url_decoded() {
        assert_eq!(
            parse_deeplink("gphotos://photo/AF1%2FQip%20x?account=someone%40example.com&x=1"),
            Some((
                Kind::Photo(Some("someone@example.com".into())),
                "AF1/Qip x".into()
            ))
        );
        assert_eq!(
            parse_deeplink("gphotos://album/Album%2BKey"),
            Some((Kind::Album(None), "Album+Key".into()))
        );
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;

    #[test]
    fn dropped_file_paths_keeps_only_real_files() {
        let temp = std::env::temp_dir().join("gphotos-drop-test");
        let _ = std::fs::create_dir_all(&temp);
        let file = temp.join("photo.jpg");
        let _ = std::fs::write(&file, b"x");
        let got = dropped_file_paths(vec![file.clone(), temp.clone()]);
        assert_eq!(got, vec![file], "directory dropped alongside a file must be ignored");
        let _ = std::fs::remove_dir_all(&temp);
    }
}
