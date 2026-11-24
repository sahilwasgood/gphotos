// Google Photos library control — a desktop port of xob0t/Google-Photos-Toolkit's
// RPC surface. Auth = your own browser session cookies; every call is the same
// internal batchexecute RPC the web app itself makes.

use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

// Did the most recent rpc on THIS thread fail? List helpers collapse a failed
// rpc into an empty vec, so callers check this to tell a real empty list from a
// broken request. Thread-local keeps concurrent workers (storage scan, thumb
// fills, quota) from contaminating each other's answer.
thread_local! {
    static LAST_RPC_FAILED: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

/// True if the last `Session::rpc` call on this thread failed. Check it right
/// after a list helper: helpers return an empty vec both for a real empty list
/// and for a failed request, and this is the only way to tell them apart.
pub fn last_rpc_failed() -> bool {
    LAST_RPC_FAILED.with(|f| f.get())
}

#[derive(Clone)]
pub struct Session {
    pub cookie: String,
    pub at: String,
    pub fsid: String,
    pub bl: String,
    pub path: String,
    pub rapt: String,
    // cookie header that the fife image cdn accepts (see fifecookies.py)
    pub fife_cookie: String,
    // signed-in account (the oPEP7c wiz value IS the email)
    pub email: String,
    // browser profile the cookie came from; (jar, email) is the identity
    pub jar: String,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn cache_dir() -> std::path::PathBuf {
    super::cache_dir()
}

/// Map of `jar|email` -> cached session. One entry per browser-profile account.
pub fn sessions_path() -> std::path::PathBuf {
    cache_dir().join("sessions.json")
}

/// Registry of discovered accounts and the owner's hide/selection choices.
pub fn accounts_path() -> std::path::PathBuf {
    cache_dir().join("accounts.json")
}

fn legacy_session_path() -> std::path::PathBuf {
    cache_dir().join("session.json")
}

fn ensure_legacy_archived() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let p = legacy_session_path();
        if p.exists() {
            // the old file could belong to any account: keep it as evidence,
            // never let it become a mixed-account cache
            let dest = p.with_extension(format!("archived-{}", now_secs()));
            let _ = std::fs::rename(&p, &dest);
        }
    });
}

fn write_secret(path: &std::path::Path, body: &str) -> bool {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    // create 0600 from the start: never world-readable for a moment
    #[cfg(unix)]
    opts.mode(0o600);
    let ok = match opts.open(path) {
        Ok(mut f) => f.write_all(body.as_bytes()).is_ok(),
        Err(_) => false,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    ok
}

fn session_key(jar: &str, email: &str) -> String {
    format!("{}|{}", jar, email)
}

fn is_signed_out(html: &str) -> bool {
    html.contains("accounts.google.com/ServiceLogin") || html.contains("/v3/signin")
}

const DEFAULT_BL: &str = "boq-photosui frontend_20260101";

// Fill every RPC token from a signed-in Photos page. Shared by full capture and
// by cached-session revalidation: an imported (Windows ABE) cache starts with
// empty tokens, so revalidation must hydrate ALL of them — refilling only `at`
// left path="" and built https://photos.google.comdata/batchexecute.
fn hydrate_tokens(s: &mut Session, html: &str) {
    if let Some(v) = wiz(html, "SNlM0e") { s.at = v; }
    if let Some(v) = wiz(html, "FdrFJe") { s.fsid = v; }
    if let Some(v) = wiz(html, "cfb2h") { s.bl = v; }
    else if s.bl.is_empty() { s.bl = DEFAULT_BL.into(); }
    if let Some(v) = wiz(html, "eptZe") { s.path = v; }
    else if s.path.is_empty() { s.path = "/".into(); }
    if let Some(v) = wiz(html, "Dbw5Ud") { s.rapt = v; }
    if let Some(v) = wiz(html, "oPEP7c") { s.email = v; }
}

// A fetched sign-in page is proof the session is dead; a failed fetch (offline)
// is not. A page with neither marker is ambiguous — keep the cached session.
fn revalidate_cached(cached: Option<Session>, html: Option<String>) -> Option<Session> {
    let mut s = cached?;
    let Some(html) = html else { return Some(s) };
    if is_signed_out(&html) {
        return None;
    }
    if let Some(email) = wiz(&html, "oPEP7c") {
        if email != s.email {
            return None; // a different account answered — definite mismatch
        }
        hydrate_tokens(&mut s, &html);
        return Some(s);
    }
    Some(s)
}

// Keep a stored fife header when a revalidated cache is re-saved (an imported
// ABE export carries the OSID pair the local helper cannot decrypt). Normal
// captures never store one, so they keep recomputing the rotating pair.
fn merge_stored_fife(fresh: &mut Value, old: Option<&Value>) {
    if let Some(fife) = old
        .and_then(|v| v.get("fife_cookie"))
        .and_then(Value::as_str)
    {
        if !fife.is_empty() {
            if let Some(obj) = fresh.as_object_mut() {
                obj.insert("fife_cookie".into(), Value::String(fife.into()));
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountRecord {
    pub jar: String,
    pub email: String,
    pub name: String,
    pub hidden: bool,
    pub last_seen: i64,
    pub last_ok: i64,
}

/// One session handed over by the local Windows cookie-export extension.
/// `fife_cookie` may be empty; `fife_cookie_header()` is used in that case.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedSession {
    pub jar: String,
    pub email: String,
    pub cookie: String,
    pub fife_cookie: String,
}

pub fn load_registry() -> Vec<AccountRecord> {
    load_registry_from(&accounts_path())
}

fn load_registry_from(path: &std::path::Path) -> Vec<AccountRecord> {
    let txt = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let v: Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let Some(rows) = v.as_array() else { return vec![] };
    rows.iter()
        .filter_map(|r| {
            let g = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let jar = g("jar");
            let email = g("email");
            if jar.is_empty() || email.is_empty() {
                return None;
            }
            Some(AccountRecord {
                jar,
                email,
                name: g("name"),
                hidden: r.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                last_seen: r.get("last_seen").and_then(Value::as_i64).unwrap_or(0),
                last_ok: r.get("last_ok").and_then(Value::as_i64).unwrap_or(0),
            })
        })
        .collect()
}

pub fn save_registry(rows: &[AccountRecord]) -> bool {
    save_registry_to(&accounts_path(), rows)
}

fn save_registry_to(path: &std::path::Path, rows: &[AccountRecord]) -> bool {
    let arr: Vec<Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "jar": r.jar,
                "email": r.email,
                "name": r.name,
                "hidden": r.hidden,
                "last_seen": r.last_seen,
                "last_ok": r.last_ok,
            })
        })
        .collect();
    write_secret(path, &Value::Array(arr).to_string())
}

const LIST_ACCOUNTS_URL: &str =
    "https://accounts.google.com/ListAccounts?gpsia=1&source=ChromiumBrowser&json=standard";

// A SID cookie marks a browser profile that actually holds Google sign-ins.
// Compare whole cookie names so "XSIDE=..." cannot masquerade as "SID".
fn has_sid_cookie(cookie: &str) -> bool {
    cookie.split(';').any(|part| {
        part.trim()
            .split_once('=')
            .map(|(name, value)| name.trim() == "SID" && !value.trim().is_empty())
            .unwrap_or(false)
    })
}

// (row position, email, name, flags explicitly confirmed the session). Position
// is only a probe hint for authuser; flags missing from a short row are unknown,
// so the account is kept. Dead rows (valid_session=0 / signed_out=1) are dropped.
fn list_account_rows(json: &str) -> Vec<(usize, String, String, bool)> {
    let text = json.trim_start();
    let body = text.strip_prefix(")]}'").map(str::trim_start).unwrap_or(text);
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let Some(rows) = v.as_array() else { return vec![] };
    let mut out = vec![];
    for (i, entry) in rows.iter().enumerate() {
        let Some(row) = entry.as_array() else { continue };
        let Some(email) = row.get(3).and_then(Value::as_str) else { continue };
        if email.is_empty() {
            continue;
        }
        let name = row
            .get(2)
            .and_then(Value::as_str)
            .or_else(|| row.get(1).and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let num = |idx: usize| match row.get(idx) {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::Bool(b)) => Some(if *b { 1 } else { 0 }),
            _ => None,
        };
        if num(9) == Some(0) || num(14) == Some(1) {
            continue;
        }
        out.push((i, email.to_string(), name, num(9).is_some() || num(14).is_some()));
    }
    out
}

fn parse_list_accounts(json: &str) -> Vec<(String, String, bool)> {
    list_account_rows(json)
        .into_iter()
        .map(|(_, email, name, confirmed)| (email, name, confirmed))
        .collect()
}

// Every profile with a SID cookie is asked which accounts it holds. Google
// reorders authuser indices, so the ListAccounts row position is only the first
// probe; each email is emitted only when that jar's photos page confirms its
// oPEP7c identity, with a full 0..8 scan as the fallback.
// Old installs stored a short jar label ("Brave-Origin", "Default") before jar
// ids became `browser|profile|db`. Resolve one to the full id so a migrated
// selection can still find its session.
pub fn resolve_legacy_jar(short: &str) -> Option<String> {
    if short.contains('|') {
        return Some(short.to_string());
    }
    collect_cookie_sources()
        .into_iter()
        .find(|(id, _)| id.split('|').any(|part| part == short || part.starts_with(short)))
        .map(|(id, _)| id)
}

pub fn discover_accounts() -> Vec<AccountRecord> {
    let mut out: Vec<AccountRecord> = vec![];
    for (jar, cookie) in collect_cookie_sources() {
        if !has_sid_cookie(&cookie) {
            continue;
        }
        let Some(json) = http("GET", LIST_ACCOUNTS_URL, &cookie, "", "https://www.google.com")
        else {
            continue;
        };
        for (pos, email, name, _) in list_account_rows(&json) {
            let hint = pos as u32;
            let mut order = vec![hint];
            for n in 0..9u32 {
                if n != hint {
                    order.push(n);
                }
            }
            let mut confirmed = false;
            for n in order {
                let Some(html) = http("GET", &home_url(n), &cookie, "", "") else {
                    continue;
                };
                if wiz(&html, "oPEP7c").as_deref() == Some(email.as_str()) {
                    confirmed = true;
                    break;
                }
            }
            if !confirmed || out.iter().any(|a| a.jar == jar && a.email == email) {
                continue;
            }
            out.push(AccountRecord {
                jar: jar.clone(),
                email,
                name,
                hidden: false,
                last_seen: now_secs(),
                last_ok: 0,
            });
        }
    }
    out
}

fn home_url(authuser: u32) -> String {
    if authuser > 0 {
        format!("https://photos.google.com/?authuser={}", authuser)
    } else {
        "https://photos.google.com/".into()
    }
}

// The legacy `scrape(authuser)` selection, preserved verbatim for the current
// main.rs callers: probe THAT authuser index on every jar, a signed-in page
// wins, and the first sign-in-shell (SNlM0e only) is the last resort. Fetch is
// injected so selection can be tested without real cookies or network.
fn legacy_probe(
    sources: &[(String, String)],
    authuser: u32,
    fife: &str,
    fetch: impl Fn(&str, &str) -> Option<String>,
) -> Option<Session> {
    let url = home_url(authuser);
    let mut fallback: Option<Session> = None;
    for (jar, cookie) in sources {
        let html = match fetch(&url, cookie) {
            Some(h) => h,
            None => continue,
        };
        let signed_in = html.contains("oPEP7c");
        if !html.contains("SNlM0e") {
            continue;
        }
        let s = Session::session_from_html(jar, cookie, &html, fife);
        if signed_in {
            return Some(s);
        }
        if fallback.is_none() {
            fallback = Some(s);
        }
    }
    fallback
}

#[derive(Clone)]
pub struct MediaItem {
    pub media_key: String,
    pub dedup_key: String,
    pub thumb: String,
    pub title: String,
    pub w: u32,
    pub h: u32,
    pub is_favorite: bool,
    pub ts: i64,
    // videos carry a duration blob in the trailing ext dict
    pub is_video: bool,
    pub duration_ms: u64,
}

#[derive(Clone)]
pub struct Album {
    pub key: String,
    pub title: String,
    pub thumb: String,
    pub count: u64,
    pub is_shared: bool,
}

impl Session {
    // collect google.com cookies from every local browser (firefox plaintext,
    // brave/chrome via gcookies.py with keyring decryption), then use whichever
    // source actually authenticates against photos.google.com
    pub fn capture() -> Option<Session> {
        Self::capture_with(0)
    }

    fn read_sessions() -> serde_json::Map<String, Value> {
        let txt = match std::fs::read_to_string(sessions_path()) {
            Ok(t) => t,
            Err(_) => return Default::default(),
        };
        match serde_json::from_str::<Value>(&txt) {
            Ok(Value::Object(m)) => m,
            _ => Default::default(),
        }
    }

    fn session_from_value(v: &Value) -> Option<Session> {
        let g = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let jar = g("jar");
        let email = g("email");
        if jar.is_empty() || email.is_empty() {
            return None;
        }
        let fife = g("fife_cookie");
        let mut path = g("path");
        // an imported cache writes "" before its first revalidation; without a
        // leading slash the RPC host and path merge into photos.google.comdata
        if path.is_empty() {
            path = "/".into();
        }
        Some(Session {
            jar,
            email,
            cookie: g("cookie"),
            at: g("at"),
            fsid: g("fsid"),
            bl: g("bl"),
            path,
            rapt: g("rapt"),
            // an imported session can carry its own fife header (Windows ABE
            // blocks the local helper); otherwise fall back to the OS helper
            fife_cookie: if fife.is_empty() { fife_cookie_header() } else { fife },
        })
    }

    fn value_from_session(&self) -> Value {
        serde_json::json!({
            "jar": self.jar,
            "email": self.email,
            "cookie": self.cookie,
            // only an imported session stores its own fife header; regular
            // captures keep recomputing the rotating OSID pair via the helper
            "at": self.at,
            "fsid": self.fsid,
            "bl": self.bl,
            "path": self.path,
            "rapt": self.rapt,
        })
    }

    // Windows 127+ ABE cannot decrypt a browser's cookie store from outside the
    // browser. A trusted local export (our documented extension) hands the
    // cookies over; store them in the same 0600 cache `capture_identity` reads,
    // keyed by jar|email, so selection and every worker work unchanged.
    pub fn import_sessions(rows: &[ImportedSession]) -> bool {
        let mut map = Self::read_sessions();
        for r in rows {
            map.insert(
                session_key(&r.jar, &r.email),
                serde_json::json!({
                    "jar": &r.jar,
                    "email": &r.email,
                    "cookie": &r.cookie,
                    "fife_cookie": &r.fife_cookie,
                    "at": "", "fsid": "", "bl": "", "path": "", "rapt": "",
                }),
            );
        }
        write_secret(&sessions_path(), &Value::Object(map).to_string())
    }

    fn load_cached(jar: &str, email: &str) -> Option<Session> {
        let map = Self::read_sessions();
        let value = map.get(&session_key(jar, email)).or_else(|| {
            // pre-rename installs cached sessions under a short jar label
            // ("Brave-Origin|email"). Match that legacy key by email so an
            // upgraded app keeps its signed-in session.
            let legacy = format!("|{}", email);
            map.iter()
                .find(|(k, _)| k.ends_with(&legacy) && !k.contains("||"))
                .map(|(_, v)| v)
        })?;
        let s = Self::session_from_value(value)?;
        let html = http("GET", &home_url(0), &s.cookie, "", "");
        revalidate_cached(Some(s), html)
    }

    fn save_cached(&self) {
        let mut map = Self::read_sessions();
        let key = session_key(&self.jar, &self.email);
        let old = map.get(&key).cloned();
        let mut fresh = self.value_from_session();
        merge_stored_fife(&mut fresh, old.as_ref());
        map.insert(key, fresh);
        write_secret(&sessions_path(), &Value::Object(map).to_string());
    }

    // Capture one selected account from its own browser profile. Reuse a
    // still-valid cache (offline-safe); otherwise find the authuser index whose
    // oPEP7c email matches. Indices are never persisted — Google reorders them.
    pub fn capture_identity(jar: &str, email: &str) -> Option<Session> {
        ensure_legacy_archived();
        if let Some(s) = Self::load_cached(jar, email) {
            return Some(s);
        }
        let cookie = collect_cookie_sources()
            .into_iter()
            .find(|(name, _)| name == jar)
            .map(|(_, c)| c)?;
        let fife = fife_cookie_header();
        for n in 0..32u32 {
            let html = match http("GET", &home_url(n), &cookie, "", "") {
                Some(h) => h,
                None => continue,
            };
            if wiz(&html, "oPEP7c").as_deref() != Some(email) || !html.contains("SNlM0e") {
                continue;
            }
            let s = Self::session_from_html(jar, &cookie, &html, &fife);
            s.save_cached();
            return Some(s);
        }
        None
    }

    fn session_from_html(jar: &str, cookie: &str, html: &str, fife: &str) -> Session {
        let mut s = Session {
            jar: jar.into(),
            cookie: cookie.into(),
            fife_cookie: fife.into(),
            at: String::new(),
            fsid: String::new(),
            bl: String::new(),
            path: String::new(),
            rapt: String::new(),
            email: String::new(),
        };
        hydrate_tokens(&mut s, html);
        s
    }

    // Legacy path for all current main.rs callers (Task 4 will rewire them):
    // cache-first at the requested authuser, then the old per-jar probe with
    // its sign-in-shell fallback. Do not "modernise" the selection here.
    pub fn capture_with(authuser: u32) -> Option<Session> {
        ensure_legacy_archived();
        if let Some(s) = Self::read_sessions().values().filter_map(Self::session_from_value).find_map(|s| {
            let html = http("GET", &home_url(authuser), &s.cookie, "", "");
            revalidate_cached(Some(s), html)
        }) {
            s.save_cached();
            return Some(s);
        }
        let fife = fife_cookie_header();
        let s = legacy_probe(&collect_cookie_sources(), authuser, &fife, |url, cookie| {
            http("GET", url, cookie, "", "")
        })?;
        s.save_cached();
        Some(s)
    }
    // Pure URL build, split out so a cached/imported session's path can be
    // checked without a network round-trip.
    fn rpc_url(&self, rpcid: &str) -> String {
        format!(
            "https://photos.google.com{}data/batchexecute?rpcids={}&source-path=/&f.sid={}&bl={}&pageId=none&rt=c{}",
            self.path,
            rpcid,
            self.fsid,
            self.bl,
            if self.rapt.is_empty() {
                String::new()
            } else {
                format!("&rapt={}", urlencode(&self.rapt))
            }
        )
    }

    // one RPC round-trip; returns the parsed payload of the wrb.fr envelope
    pub fn rpc(&self, rpcid: &str, payload: &Value) -> Option<Value> {
        LAST_RPC_FAILED.with(|f| f.set(false));
        let wrapped = serde_json::json!([[[
            rpcid,
            serde_json::to_string(payload).unwrap_or_default(),
            null,
            "generic"
        ]]]);
        let body = format!(
            "f.req={}&at={}&",
            urlencode(&wrapped.to_string()),
            urlencode(&self.at)
        );
        let url = self.rpc_url(rpcid);
        let out = match http("POST", &url, &self.cookie, &body, "") {
            Some(o) => o,
            None => {
                LAST_RPC_FAILED.with(|f| f.set(true));
                return None;
            }
        };
        // batchexecute bodies are length-prefixed chunks; a long record is
        // split across several chunks that must be joined before parsing
        let mut batch_error = false;
        for msg in dechunk(out) {
            // a reassembled buffer can hold several single-line json docs
            for line in msg.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("[\"e\",") {
                    if let Ok(e) = serde_json::from_str::<Value>(trimmed) {
                        let code = e.get(1).and_then(Value::as_u64)
                            .or_else(|| e.get(1).and_then(Value::as_str).and_then(|s| s.parse().ok()))
                            .unwrap_or(0);
                        if code > 0 {
                            batch_error = true;
                        }
                    }
                }
                let line = line.trim();
                if !line.contains("\"wrb.fr\"") || !line.contains(rpcid) {
                    continue;
                }
                if let Ok(env) = serde_json::from_str::<Value>(line) {
                    // envelope: [["wrb.fr","<rpcid>","<payload-json>",null,null,"<status>"]]
                    // a refused write answers with a null payload and/or a
                    // non-zero status at [5]; treating those as success made
                    // every mutator lie about what happened
                    let row = &env[0];
                    let num = |v: Option<&Value>| -> u64 {
                        v.and_then(Value::as_u64)
                            .or_else(|| v.and_then(Value::as_str).and_then(|s| s.parse().ok()))
                            .unwrap_or(0)
                    };
                    let err_text = row.get(3).and_then(Value::as_str).unwrap_or("");
                    // status arrives as a plain string, a number, or a
                    // one-element array depending on the rpc — normalise all
                    let s5 = row.get(5);
                    let status = num(
                        s5.and_then(Value::as_array)
                            .and_then(|a| a.first())
                            .or(s5),
                    );
                    let batch_err = env.as_array().map(|a| {
                        a.iter().any(|r| {
                            r.get(0).and_then(Value::as_str) == Some("e")
                                && num(r.get(1)) > 0
                        })
                    }).unwrap_or(false);
                    if !err_text.is_empty() || status > 0 || batch_err {
                        if std::env::var("GPHOTOS_RPCDEBUG").is_ok() {
                            eprintln!("rpc {} refused: {}", rpcid, line.chars().take(400).collect::<String>());
                        }
                        continue;
                    }
                    // a clean null/empty payload is still a valid answer for
                    // many writes — only error signals mean failure
                    let parsed = row
                        .get(2)
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or(Value::Null);
                    return Some(parsed);
                }
            }
        }
        if batch_error {
            if std::env::var("GPHOTOS_RPCDEBUG").is_ok() {
                eprintln!("rpc {} refused: batch error row", rpcid);
            }
            LAST_RPC_FAILED.with(|f| f.set(true));
            return None;
        }
        LAST_RPC_FAILED.with(|f| f.set(true));
        None
    }
}

// Interpreters to try, in order. Windows' official launcher is `py -3`; asking
// for it on other OSes would just find an unrelated binary. Pure so the order
// can be fixture-tested on any host.
fn python_candidates(os_name: &str) -> Vec<(&'static str, Vec<&'static str>)> {
    let mut v = vec![("python3", vec![]), ("python", vec![])];
    if os_name == "windows" {
        v.push(("py", vec!["-3"]));
    }
    v
}

// Run a bundled Python helper with the first interpreter that works. Stdout is
// parsed by callers and may contain cookies, so it is never logged; only our
// own content-free helper notices (fixed "gphotos-cookie-helper:" prefix) are
// forwarded from stderr, once.
pub(crate) fn run_python_script(script: &std::path::Path) -> Option<String> {
    let os_name = if cfg!(target_os = "windows") { "windows" } else { "other" };
    for (prog, pre) in python_candidates(os_name) {
        let Ok(out) = Command::new(prog).args(&pre).arg(script).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        static HELPER_NOTICE_WARNED: std::sync::Once = std::sync::Once::new();
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            if line.starts_with("gphotos-cookie-helper:") {
                HELPER_NOTICE_WARNED.call_once(|| eprintln!("{line}"));
            }
        }
        return Some(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    None
}

// fife cdn auth = session cookies PLUS the fife-scoped OSID pair that the
// regular cookie dump filters out; fifecookies.py emits the merged header
pub fn fife_cookie_header() -> String {
    // cached: the keyring-decrypt python helper takes a second or two, and
    // brave only rotates these timestamps slowly
    static CACHE: std::sync::OnceLock<std::sync::Mutex<(Instant, String)>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        std::sync::Mutex::new((Instant::now() - Duration::from_secs(3600), String::new()))
    });
    let mut g = cache.lock().unwrap();
    if !g.1.is_empty() && g.0.elapsed() < Duration::from_secs(600) {
        return g.1.clone();
    }
    let script = super::app_root().join("fifecookies.py");
    let v = run_python_script(&script)
        .map(|s| s.trim().replace('\n', "; "))
        .unwrap_or_default();
    if !v.is_empty() {
        *g = (Instant::now(), v.clone());
    }
    v
}

fn wiz(html: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":\"", key);
    let start = html.find(&pat)? + pat.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// reassemble google's length-prefixed chunked batchexecute body
fn dechunk(body: String) -> Vec<String> {
    let mut msgs: Vec<String> = vec![];
    // skip past the )]}' header
    let start = body.find(")]}'").map(|p| p + 4).unwrap_or(0);
    let bytes = body.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        // skip whitespace/newlines
        while i < bytes.len() && (bytes[i] == b'\n' || bytes[i] == b'\r' || bytes[i] == b' ') {
            i += 1;
        }
        // read the length line
        let line_end = match bytes[i..].iter().position(|&b| b == b'\n') {
            Some(p) => i + p,
            None => break,
        };
        let n: usize = match std::str::from_utf8(&bytes[i..line_end])
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            Some(n) => n,
            None => break,
        };
        i = line_end + 1;
        if n > bytes.len() - i {
            break;
        }
        let chunk = String::from_utf8_lossy(&bytes[i..i + n]).to_string();
        i += n;
        // a chunk starting a JSON value begins a new message; otherwise it
        // continues the previous one
        if chunk.starts_with('[') || chunk.starts_with('{') {
            msgs.push(chunk);
        } else if let Some(last) = msgs.last_mut() {
            last.push_str(&chunk);
        } else {
            msgs.push(chunk);
        }
    }
    msgs
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// escape a value for a curl -K config line (quoted with double quotes)
pub fn curl_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // CR/LF would be decoded back into real header line breaks by
            // curl — drop them so a hostile value cannot inject a header
            '\n' | '\r' => {}
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

// run curl with the session cookie supplied via -K - on stdin, so no secret
// appears in the process command line. `extra` holds any other config lines
// (url, header, output, write-out, ...) already curl-quoted by the caller.
pub fn curl_with_cookie(extra: &[String], cookie: &str) -> Option<Vec<u8>> {
    let mut cfg = format!("header = {}\n", curl_quote(&format!("Cookie: {}", cookie)));
    for line in extra {
        cfg.push_str(line);
        cfg.push('\n');
    }
    let mut cmd = std::process::Command::new("curl");
    cmd.arg("-s")
        .arg("-K")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let Some(mut inp) = child.stdin.take() {
        use std::io::Write;
        let _ = inp.write_all(cfg.as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    if out.status.success() {
        Some(out.stdout)
    } else {
        None
    }
}

fn http(method: &str, url: &str, cookie: &str, body: &str, origin: &str) -> Option<String> {
    // Everything sensitive (session cookie, at-token, query secrets) goes to
    // curl via a config on stdin — never in argv where any local process can
    // read it from /proc/<pid>/cmdline.
    let mut cfg = format!(
        "url = {}\nheader = {}\nheader = {}\n",
        curl_quote(url),
        curl_quote(&format!("Cookie: {}", cookie)),
        curl_quote("User-Agent: Mozilla/5.0 (X11; Linux x86_64) gphotos-app"),
    );
    if !origin.is_empty() {
        cfg.push_str(&format!("header = {}\n", curl_quote(&format!("Origin: {}", origin))));
    }
    if method == "POST" {
        cfg.push_str(&format!(
            "request = POST\nheader = {}\ndata = {}\n",
            curl_quote("Content-Type: application/x-www-form-urlencoded;charset=UTF-8"),
            curl_quote(body),
        ));
    }
    let mut cmd = std::process::Command::new("curl");
    cmd.arg("-s")
        .arg("--max-time")
        .arg("30")
        .arg("-K")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let Some(mut inp) = child.stdin.take() {
        use std::io::Write;
        let _ = inp.write_all(cfg.as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        None
    }
}

// every browser source that gcookies.py can extract google cookies from
fn collect_cookie_sources() -> Vec<(String, String)> {
    let script = super::app_root().join("gcookies.py");
    let Some(text) = run_python_script(&script) else {
        return vec![];
    };
    let mut sources: Vec<(String, String)> = vec![];
    let mut cur_name = "unknown".to_string();
    let mut cur_pairs: Vec<(String, String)> = vec![];
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# === ") {
            if !cur_pairs.is_empty() {
                sources.push((cur_name.clone(), cur_pairs.iter().map(|(n, v)| format!("{}={}", n, v)).collect::<Vec<_>>().join("; ")));
            }
            cur_name = rest.trim().to_string();
            cur_pairs.clear();
        } else if let Some((n, v)) = line.split_once('=') {
            cur_pairs.push((n.trim().to_string(), v.trim().to_string()));
        }
    }
    if !cur_pairs.is_empty() {
        sources.push((cur_name, cur_pairs.iter().map(|(n, v)| format!("{}={}", n, v)).collect::<Vec<_>>().join("; ")));
    }
    sources
}


// ---- parsed pages -----------------------------------------------------------

fn parse_item(d: &Value) -> Option<MediaItem> {
    let media_key = d.get(0)?.as_str()?.to_string();
    let dedup_key = d.get(3).and_then(Value::as_str).unwrap_or("").to_string();
    let thumb = d
        .get(1)
        .and_then(|t| t.get(0))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let w = d.get(1).and_then(|t| t.get(1)).and_then(Value::as_u64).unwrap_or(0) as u32;
    let h = d.get(1).and_then(|t| t.get(2)).and_then(Value::as_u64).unwrap_or(0) as u32;
    let mut ts = d.get(2).and_then(Value::as_i64).unwrap_or(0);
    if ts > 100_000_000_000 {
        ts /= 1000; // milliseconds flavor
    }
    let ext = d.as_array().and_then(|a| a.last());
    let title = ext
        .and_then(|t| t.get("72930366"))
        .and_then(|t| t.get(1))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_favorite = ext
        .and_then(|t| t.get("163238866"))
        .and_then(|t| t.get(0))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dur = ext.and_then(|t| t.get("76647426"));
    let is_video = dur.is_some();
    let duration_ms = dur
        .and_then(|d| d.get(0))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if dedup_key.is_empty() && thumb.is_empty() {
        return None;
    }
    Some(MediaItem {
        media_key,
        dedup_key,
        thumb,
        title,
        w,
        h,
        is_favorite,
        ts,
        is_video,
        duration_ms,
    })
}

// item array + page token land in different slots per endpoint (trash puts the
// rows at [0], locked and albums at [1]), so find whichever slot holds rows
fn parse_generic_page(v: &Value) -> (Vec<MediaItem>, Option<String>) {
    let mut items: Vec<MediaItem> = vec![];
    let mut next: Option<String> = None;
    if let Some(slots) = v.as_array() {
        for slot in slots {
            match slot {
                Value::Array(rows) if rows.iter().any(|r| r.is_array()) => {
                    if items.is_empty() {
                        items = rows.iter().filter_map(parse_item).collect();
                    }
                }
                Value::String(s) if !s.is_empty() && next.is_none() => {
                    if s.len() > 16 {
                        next = Some(s.clone());
                    }
                }
                _ => {}
            }
        }
    }
    (items, next)
}

fn parse_albums(v: &Value) -> (Vec<Album>, Option<String>) {
    let items = v
        .get(0)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    Some(Album {
                        key: d.get(0)?.as_str()?.to_string(),
                        title: d
                            .as_array()
                            .and_then(|a| a.last())
                            .and_then(|t| t.get("72930366"))
                            .and_then(|t| t.get(1))
                            .and_then(Value::as_str)
                            .unwrap_or("untitled album")
                            .to_string(),
                        thumb: d
                            .get(1)
                            .and_then(|t| t.get(0))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        count: d
                            .as_array()
                            .and_then(|a| a.last())
                            .and_then(|t| t.get("72930366"))
                            .and_then(|t| t.get(3))
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        is_shared: d
                            .as_array()
                            .and_then(|a| a.last())
                            .and_then(|t| t.get("72930366"))
                            .and_then(|t| t.get(4))
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let next = v
        .get(1)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);
    (items, next)
}

// ---- the feats --------------------------------------------------------------

// source 1 = library, 2 = archive; the timeline the web app shows excludes
// archived photos, which is why "archive" needs this rpc and not EzkLib
fn timeline(s: &Session, page: Option<&str>, source: i64) -> (Vec<MediaItem>, Option<String>) {
    s.rpc(
        "lcxiM",
        &serde_json::json!([page, null, 200, null, 1, source]),
    )
    .map(|v| parse_generic_page(&v))
    .unwrap_or_default()
}

pub fn recent(s: &Session, page: Option<&str>) -> (Vec<MediaItem>, Option<String>) {
    timeline(s, page, 1)
}

pub fn archive(s: &Session, page: Option<&str>) -> (Vec<MediaItem>, Option<String>) {
    timeline(s, page, 2)
}

pub fn favorites(s: &Session, page: Option<&str>) -> (Vec<MediaItem>, Option<String>) {
    s.rpc(
        "EzkLib",
        &serde_json::json!(["Favorites", [[5, "8", 0, 9]], page]),
    )
    .map(|v| parse_generic_page(&v))
    .unwrap_or_default()
}

pub fn search(s: &Session, query: &str, page: Option<&str>) -> (Vec<MediaItem>, Option<String>) {
    s.rpc("EzkLib", &serde_json::json!([query, null, page]))
        .map(|v| parse_generic_page(&v))
        .unwrap_or_default()
}

pub fn trash(s: &Session, page: Option<&str>) -> (Vec<MediaItem>, Option<String>) {
    s.rpc("zy0IHe", &serde_json::json!([page]))
        .map(|v| parse_generic_page(&v))
        .unwrap_or_default()
}

pub fn albums(s: &Session, page: Option<&str>) -> (Vec<Album>, Option<String>) {
    s.rpc(
        "Z5xsfc",
        &serde_json::json!([page, null, null, null, 1, null, null, 100, [2], 5]),
    )
    .map(|v| parse_albums(&v))
    .unwrap_or_default()
}

pub fn album_items(s: &Session, key: &str, page: Option<&str>) -> (Vec<MediaItem>, Option<String>) {
    s.rpc("snAcKc", &serde_json::json!([key, page, null, null]))
        .map(|v| parse_generic_page(&v))
        .unwrap_or_default()
}

// state = 1 trash / 3 restore, mirrors the toolkit's moveItemsToTrash pair
pub fn set_trashed(s: &Session, dedup_keys: &[String], to_trash: bool) -> bool {
    let (state, mode) = if to_trash { (1, 3) } else { (3, 2) };
    s.rpc(
        "XwAOJf",
        &serde_json::json!([null, state, dedup_keys, mode]),
    )
    .is_some()
}

// add media keys to an existing album (or create one by name)
pub fn add_to_album(s: &Session, media_keys: &[String], album_key: Option<&str>, album_name: Option<&str>) -> bool {
    let payload = match (album_key, album_name) {
        (Some(k), _) => serde_json::json!([media_keys, k]),
        (None, Some(n)) => serde_json::json!([media_keys, null, n]),
        _ => return false,
    };
    s.rpc("E1Cajb", &payload).is_some()
}

pub fn remove_from_album(s: &Session, album_item_keys: &[String]) -> bool {
    s.rpc("ycV3Nd", &serde_json::json!([album_item_keys]))
        .is_some()
}

pub fn set_archived(s: &Session, dedup_keys: &[String], archive: bool) -> bool {
    let code = if archive { 1 } else { 2 };
    let mapped: Vec<Value> = dedup_keys
        .iter()
        .map(|k| serde_json::json!([null, [code], [null, k]]))
        .collect();
    s.rpc("w7TP3c", &serde_json::json!([mapped, null, 1]))
        .is_some()
}

pub fn set_favorite(s: &Session, dedup_keys: &[String], favorite: bool) -> bool {
    let code = if favorite { 1 } else { 2 };
    let mapped: Vec<Value> = dedup_keys
        .iter()
        .map(|k| serde_json::json!([null, k]))
        .collect();
    s.rpc("Ftfh0", &serde_json::json!([mapped, [code]]))
        .is_some()
}

// ---- round 2: the rest of the toolkit surface -------------------------------

#[derive(Clone)]
pub struct SharedLink {
    pub key: String,
    pub count: u64,
}

pub struct Quota {
    pub used: u64,
    pub available: u64,
    pub gphotos: u64,
}

#[derive(Clone)]
pub struct BulkInfo {
    // where the item came from: 1 phone, 2 web, 7 drive, 8 computer, 11 gmail
    pub source: u64,
    pub media_key: String,
    pub filename: String,
    pub description: String,
    pub size: u64,
    pub timestamp: i64,
    pub tz_offset: i64,
    pub takes_up_space: Option<bool>,
    pub space_flag: Option<u64>,
    pub orig_flag: Option<u64>,
    pub original_quality: Option<bool>,
}

// per-item detail beyond bulk info: camera, place, owner, upload source
#[derive(Clone)]
pub struct ItemExt {
    pub filename: String,
    pub camera: String,
    pub place: String,
    pub coords: Option<(f64, f64)>,
    pub owner: String,
    pub source: String,
    pub albums: Vec<String>,
}

fn at<'a>(v: &'a Value, path: &[usize]) -> Option<&'a Value> {
    let mut cur = v;
    for i in path {
        cur = cur.get(i)?;
    }
    Some(cur)
}

pub fn item_ext(s: &Session, media_key: &str) -> Option<ItemExt> {
    let v = s.rpc("fDcn4b", &serde_json::json!([media_key, 1, null, null, 1]))?;
    // response shape: [[item_data, ...], ...]
    let item = v
        .get(0)
        .and_then(|x| x.get(0))
        .filter(|x| x.is_array())
        .or_else(|| v.get(0))?;
    let text = |path: &[usize]| {
        at(item, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let coords = at(item, &[9, 0])
        .and_then(Value::as_array)
        .filter(|a| a.len() >= 2)
        .and_then(|a| Some((a[0].as_f64()?, a[1].as_f64()?)));
    let coords = coords.or_else(|| {
        at(item, &[13, 0])
            .and_then(Value::as_array)
            .filter(|a| a.len() >= 2)
            .and_then(|a| Some((a[0].as_f64()?, a[1].as_f64()?)))
    });
    let place = text(&[13, 2, 0, 1, 0, 0]);
    let camera = match at(item, &[23]) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let source_code = at(item, &[27, 0]).and_then(Value::as_u64).unwrap_or(0);
    let source = match source_code {
        1 => "phone",
        2 => "web",
        3 => "shared with you",
        4 => "partner",
        7 => "drive",
        8 => "computer",
        11 => "gmail",
        _ => "",
    }
    .to_string();
    let owner = text(&[27, 3, 0, 11, 0]);
    let owner = if owner.is_empty() { text(&[28, 11, 0]) } else { owner };
    let albums = at(item, &[19])
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    // album rows end with a dict keyed "72930366" holding the title
                    r.as_array()
                        .and_then(|a| a.last())
                        .and_then(|d| d.get("72930366"))
                        .and_then(|d| d.get(1))
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .filter(|t: &String| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(ItemExt {
        filename: text(&[2]),
        camera,
        place,
        coords,
        owner,
        source,
        albums,
    })
}

// shared links list
pub fn links(s: &Session, page: Option<&str>) -> (Vec<SharedLink>, Option<String>) {
    let v = s.rpc("F2A0H", &serde_json::json!([page, null, 2, null, 3]));
    let Some(v) = v else { return (vec![], None) };
    let items = v
        .get(0)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    Some(SharedLink {
                        key: d.get(17)?.as_str()?.to_string(),
                        count: d.get(3).and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let next = v
        .get(1)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);
    (items, next)
}

// locked folder: items at [1], next token at [0] (opposite of trash)
pub fn locked(s: &Session, page: Option<&str>) -> (Vec<MediaItem>, Option<String>) {
    s.rpc("nMFwOc", &serde_json::json!([page]))
        .map(|v| parse_generic_page(&v))
        .unwrap_or_default()
}

// move into / out of the locked folder
pub fn lock_items(s: &Session, dedup_keys: &[String]) -> bool {
    s.rpc("StLnCe", &serde_json::json!([dedup_keys, []])).is_some()
}
pub fn unlock_items(s: &Session, dedup_keys: &[String]) -> bool {
    s.rpc("Pp2Xxe", &serde_json::json!([dedup_keys])).is_some()
}

// storage numbers
pub fn quota(s: &Session) -> Option<Quota> {
    let v = s.rpc("EzwWhf", &serde_json::json!([]))?;
    let q = v.get(6)?;
    Some(Quota {
        used: q.get(0).and_then(Value::as_u64).unwrap_or(0),
        available: q.get(1).and_then(Value::as_u64).unwrap_or(0),
        gphotos: q.get(3).and_then(Value::as_u64).unwrap_or(0),
    })
}

// filenames / sizes / descriptions / original-quality flags
// (official field map: parser.ts itemBulkMediaInfoParse — mask must be 35
// wide with [] at 24 and 34, or the rpc answers nothing)
// the same call, unparsed: for checking what the fields really mean
pub fn bulk_raw(s: &Session, media_keys: &[String]) -> Option<Value> {
    let mapped: Vec<Value> = media_keys.iter().map(|k| serde_json::json!([k])).collect();
    let mut mask = vec![Value::Null; 35];
    mask[24] = serde_json::json!([]);
    mask[34] = serde_json::json!([]);
    s.rpc("EWgK9e", &serde_json::json!([[[mapped], [mask]]]))
}

pub fn bulk_info(s: &Session, media_keys: &[String]) -> Vec<BulkInfo> {
    let mapped: Vec<Value> = media_keys.iter().map(|k| serde_json::json!([k])).collect();
    let mut mask = vec![Value::Null; 35];
    mask[24] = serde_json::json!([]);
    mask[34] = serde_json::json!([]);
    let payload = serde_json::json!([[[mapped], [mask]]]);
    let Some(v) = s.rpc("EWgK9e", &payload) else { return vec![] };
    let Some(rows) = v.get(0).and_then(|r| r.get(1)).and_then(Value::as_array) else {
        return vec![];
    };
    rows.iter()
        .filter_map(|d| {
            let info = d.get(1)?;
            let last = info.as_array().and_then(|a| a.last());
            Some(BulkInfo {
                source: 0,
                media_key: d.get(0)?.as_str()?.to_string(),
                filename: info.get(3).and_then(Value::as_str).unwrap_or("").to_string(),
                description: info.get(2).and_then(Value::as_str).unwrap_or("").to_string(),
                size: info.get(9).and_then(Value::as_u64).unwrap_or(0),
                timestamp: info.get(6).and_then(Value::as_i64).unwrap_or(0),
                tz_offset: info.get(7).and_then(Value::as_i64).unwrap_or(0),
                takes_up_space: last
                    .and_then(|t| t.get(0))
                    .map(|c| c.as_u64() == Some(1)),
                original_quality: last
                    .and_then(|t| t.get(2))
                    .map(|c| c.as_u64() == Some(2)),
                space_flag: last.and_then(|t| t.get(0)).and_then(Value::as_u64),
                orig_flag: last.and_then(|t| t.get(2)).and_then(Value::as_u64),
            })
        })
        .collect()
}

// ---- sharing and album housekeeping ----------------------------------------
// Both RPCs below were reverse-engineered from the live web client. They are
// small and single-purpose; a failure is reported, never guessed at.

pub fn create_album(s: &Session, name: &str) -> Option<String> {
    // the create RPC answers with an opaque id; reading the album list back
    // by name is slower but doesn't depend on guessing the response shape
    s.rpc("OXvT9d", &serde_json::json!([name, null, 2]))?;
    let (al, _) = albums(s, None);
    al.iter().find(|a| a.title == name).map(|a| a.key.clone())
}

pub fn delete_album(s: &Session, album_key: &str) -> bool {
    // deleting an album never deletes the photos inside it
    s.rpc("nV6Qv", &serde_json::json!([null, null, [[album_key, null, 1]]]))
        .is_some()
}

pub fn create_share_link(s: &Session, media_keys: &[String]) -> Option<String> {
    let keys: Vec<Value> = media_keys.iter().map(|k| serde_json::json!([[k]])).collect();
    let payload = serde_json::json!([
        null, null,
        [null, 1, null, null, 1, null,
         [[[1, 1], 0], [[1, 2], 0], [[2, 1], 1], [[2, 2], 1], [[3, 1], 1]]],
        [2, null, keys, null, null, null, [1], 0, null, null, null, null, null, 0],
        null, null, null, null, [1, 2, 3, 5, 6]
    ]);
    let v = s.rpc("SFKp8c", &payload)?;
    v.get(1).and_then(Value::as_str).map(str::to_string)
}

pub fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    for (bin, arg) in [("xclip", "-selection"), ("xsel", "--clipboard")] {
        let mut args = vec![arg];
        if bin == "xclip" {
            args.push("clipboard");
        }
        if let Ok(mut ch) = Command::new("timeout")
            .arg("3")
            .arg(bin)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut inp) = ch.stdin.take() {
                let ok = inp.write_all(text.as_bytes()).is_ok();
                drop(inp);
                let _ = ch.wait();
                if ok {
                    return true;
                }
            }
        }
    }
    false
}

// ---- the whole library, measured -------------------------------------------
//
// One row per item in every list that can hold one, with what Google says
// about its bytes. This is the only honest answer to "what eats my storage":
// the timeline alone misses archived, binned and locked items.

#[derive(Clone)]
pub struct ScanRow {
    pub item: MediaItem,
    pub info: BulkInfo,
    pub list: &'static str,
}

type Walk = fn(&Session, Option<&str>) -> (Vec<MediaItem>, Option<String>);

pub fn scan_all(s: &Session) -> Vec<ScanRow> {
    let max_pages: usize = std::env::var("GPHOTOS_AUDIT_PAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let sources: [(&'static str, Walk); 5] = [
        ("timeline", recent),
        ("archive", archive),
        ("locked", locked),
        ("favourites", favorites),
        ("trash", trash),
    ];
    let mut items: Vec<(MediaItem, &'static str)> = vec![];
    let mut seen = std::collections::HashSet::new();
    for (name, f) in sources {
        let mut page: Option<String> = None;
        for _ in 0..max_pages {
            let (got, next) = f(s, page.as_deref());
            let n = got.len();
            for it in got {
                if seen.insert(it.media_key.clone()) {
                    items.push((it, name));
                }
            }
            if n == 0 {
                break;
            }
            page = next;
            if page.is_none() {
                break;
            }
        }
    }
    // per-item detail: 35 keys per rpc (the server's cap), four calls at a time
    let keys: Vec<String> = items.iter().map(|(i, _)| i.media_key.clone()).collect();
    let queue = std::sync::Arc::new(std::sync::Mutex::new(
        keys.chunks(35).map(|c| c.to_vec()).collect::<Vec<_>>(),
    ));
    let found = std::sync::Arc::new(std::sync::Mutex::new(
        Vec::<(String, BulkInfo)>::new(),
    ));
    let hs: Vec<_> = (0..4)
        .map(|_| {
            let q = queue.clone();
            let f = found.clone();
            let sc = s.clone();
            std::thread::spawn(move || loop {
                let job = q.lock().unwrap().pop();
                let Some(k) = job else { return };
                for b in bulk_info(&sc, &k) {
                    f.lock().unwrap().push((b.media_key.clone(), b));
                }
            })
        })
        .collect();
    for h in hs {
        let _ = h.join();
    }
    let mut by_key: std::collections::HashMap<String, BulkInfo> = match std::sync::Arc::try_unwrap(found)
    {
        Ok(m) => m.into_inner().unwrap_or_default().into_iter().collect(),
        Err(_) => Default::default(),
    };
    items
        .into_iter()
        .filter_map(|(item, list)| {
            let info = by_key.remove(&item.media_key)?;
            Some(ScanRow { item, info, list })
        })
        .collect()
}

// 1_234_567 -> "1,234,567"
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn human(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.2} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes > 0 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        "0 B".into()
    }
}

fn day_label(ts: i64) -> String {
    if ts <= 0 {
        return "-".into();
    }
    const M: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    format!("{} {} {}", d, M[((m.max(1) - 1) % 12) as usize], y)
}

// the report, in words, for the page and the terminal alike — one source of
// truth so the numbers can never disagree
pub fn stats_lines(st: &StorageStats) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    if st.total_n == 0 {
        return out;
    }
    let pc = |part: u64| {
        if st.total_bytes == 0 {
            0.0
        } else {
            part as f64 * 100.0 / st.total_bytes as f64
        }
    };
    out.push((
        "measured".into(),
        format!(
            "{} items — {} photos, {} videos",
            thousands(st.total_n as u64),
            thousands(st.photo_n as u64),
            thousands(st.video_n as u64)
        ),
    ));
    out.push((
        "together they weigh".into(),
        format!(
            "{} — photos {}, videos {}",
            human(st.total_bytes),
            human(st.photo_bytes),
            human(st.video_bytes)
        ),
    ));
    if st.charged_n == 0 {
        out.push((
            "counted toward your storage".into(),
            format!("nothing — 0 of them take space, 0 B of {}", human(st.total_bytes)),
        ));
    } else {
        out.push((
            "counted toward your storage".into(),
            format!(
                "{} items · {} ({:.0}% of the bytes you keep here)",
                thousands(st.charged_n as u64),
                human(st.charged_bytes),
                pc(st.charged_bytes)
            ),
        ));
    }
    out.push((
        "storage setting".into(),
        st.space_flags
            .iter()
            .map(|(k, n, b)| format!("flag {k} · {} items · {}", thousands(*n as u64), human(*b)))
            .collect::<Vec<_>>()
            .join("  ·  "),
    ));
    out.push((
        "average item".into(),
        format!(
            "{}  ·  a video averages {}",
            human(st.total_bytes / st.total_n.max(1) as u64),
            human(st.video_bytes / st.video_n.max(1) as u64)
        ),
    ));
    if st.video_ms > 0 {
        let secs = st.video_ms as f64 / 1000.0;
        out.push((
            "video bitrate".into(),
            format!(
                "{} of video over {} h {} min — {:.1} Mbps overall",
                human(st.video_bytes),
                (secs / 3600.0) as u64,
                ((secs as u64 % 3600) / 60),
                st.video_bytes as f64 * 8.0 / secs / 1_000_000.0
            ),
        ));
    }
    if let Some((name, _, size, ts, video)) = st.top.first() {
        out.push((
            "the single biggest file".into(),
            format!(
                "{} · {} · {}{}",
                human(*size),
                if name.is_empty() { "no filename" } else { name },
                day_label(*ts),
                if *video { " · video" } else { "" }
            ),
        ));
    }
    let over100: u64 = st
        .buckets
        .iter()
        .take(4)
        .map(|(_, _, b)| *b)
        .sum();
    let over100_n: usize = st.buckets.iter().take(4).map(|(_, n, _)| *n).sum();
    out.push((
        "over 100 MB".into(),
        format!(
            "{} items · {} ({:.0}% of all the bytes you keep here)",
            thousands(over100_n as u64),
            human(over100),
            pc(over100)
        ),
    ));
    out.push((
        "kept from".into(),
        format!("{} to {}", day_label(st.oldest), day_label(st.newest)),
    ));
    if !st.lists.is_empty() {
        out.push((
            "where they live".into(),
            st.lists
                .iter()
                .map(|(k, n, b)| format!("{k} {} · {}", thousands(*n as u64), human(*b)))
                .collect::<Vec<_>>()
                .join("  ·  "),
        ));
    }
    if !st.years.is_empty() {
        let mut ys: Vec<&(i64, usize, u64)> = st.years.iter().collect();
        ys.sort_by(|a, b| b.2.cmp(&a.2));
        out.push((
            "the heaviest years".into(),
            ys.iter()
                .take(4)
                .map(|(y, n, b)| format!("{y}: {} · {} items", human(*b), thousands(*n as u64)))
                .collect::<Vec<_>>()
                .join("  ·  "),
        ));
    }
    out.push((
        "size spread".into(),
        st.buckets
            .iter()
            .map(|(label, n, b)| format!("{label}: {} · {}", thousands(*n as u64), human(*b)))
            .collect::<Vec<_>>()
            .join("  ·  "),
    ));
    if st.unknown_size_n > 0 {
        out.push((
            "no size reported".into(),
            format!("{} items", thousands(st.unknown_size_n as u64)),
        ));
    }
    out
}

#[derive(Clone, Default)]
pub struct StorageStats {
    pub total_n: usize,
    pub total_bytes: u64,
    pub photo_n: usize,
    pub photo_bytes: u64,
    pub video_n: usize,
    pub video_bytes: u64,
    pub charged_n: usize,
    pub charged_bytes: u64,
    pub unknown_size_n: usize,
    pub space_flags: Vec<(String, usize, u64)>,
    pub orig_flags: Vec<(String, usize, u64)>,
    // (label, items, bytes) biggest bucket first
    pub buckets: Vec<(&'static str, usize, u64)>,
    pub lists: Vec<(String, usize, u64)>,
    pub years: Vec<(i64, usize, u64)>,
    // (filename, media_key, bytes, ts, is_video) biggest first
    pub top: Vec<(String, String, u64, i64, bool)>,
    pub oldest: i64,
    pub newest: i64,
    pub video_ms: u64,
}

pub fn storage_stats(rows: &[ScanRow]) -> StorageStats {
    let mut st = StorageStats::default();
    let mut flags: std::collections::BTreeMap<String, (usize, u64)> = Default::default();
    let mut oflags: std::collections::BTreeMap<String, (usize, u64)> = Default::default();
    let mut lists: std::collections::BTreeMap<String, (usize, u64)> = Default::default();
    let mut years: std::collections::BTreeMap<i64, (usize, u64)> = Default::default();
    let mut sorted: Vec<&ScanRow> = rows.iter().collect();
    sorted.sort_by(|a, b| b.info.size.cmp(&a.info.size));
    st.oldest = i64::MAX;
    for r in rows {
        let size = r.info.size;
        st.total_n += 1;
        st.total_bytes += size;
        if size == 0 {
            st.unknown_size_n += 1;
        }
        if r.info.takes_up_space == Some(true) {
            st.charged_n += 1;
            st.charged_bytes += size;
        }
        if r.item.is_video {
            st.video_n += 1;
            st.video_bytes += size;
            st.video_ms += r.item.duration_ms;
        } else {
            st.photo_n += 1;
            st.photo_bytes += size;
        }
        let f = flags
            .entry(
                r.info
                    .space_flag
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
            )
            .or_default();
        f.0 += 1;
        f.1 += size;
        let o = oflags
            .entry(
                r.info
                    .orig_flag
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
            )
            .or_default();
        o.0 += 1;
        o.1 += size;
        let fl = lists.entry(r.list.to_string()).or_default();
        fl.0 += 1;
        fl.1 += size;
        let ts = r.info.timestamp / 1000;
        if ts > 0 {
            st.oldest = st.oldest.min(ts);
            st.newest = st.newest.max(ts);
            let y = civil_from_days(ts.div_euclid(86400)).0;
            let ny = years.entry(y).or_default();
            ny.0 += 1;
            ny.1 += size;
        }
    }
    if st.oldest == i64::MAX {
        st.oldest = 0;
    }
    st.space_flags = flags
        .into_iter()
        .map(|(k, (n, b))| (k, n, b))
        .collect();
    st.orig_flags = oflags
        .into_iter()
        .map(|(k, (n, b))| (k, n, b))
        .collect();
    st.lists = lists.into_iter().map(|(k, (n, b))| (k, n, b)).collect();
    st.years = years.into_iter().map(|(k, (n, b))| (k, n, b)).collect();
    let buckets: [(&'static str, u64); 7] = [
        ("1 GB and up", 1_073_741_824),
        ("500 MB to 1 GB", 536_870_912),
        ("250 to 500 MB", 262_144_000),
        ("100 to 250 MB", 104_857_600),
        ("50 to 100 MB", 52_428_800),
        ("10 to 50 MB", 10_485_760),
        ("under 10 MB", 0),
    ];
    for (i, (label, floor)) in buckets.iter().enumerate() {
        let ceil = if i == 0 { u64::MAX } else { buckets[i - 1].1 };
        let mut n = 0;
        let mut b = 0;
        for r in rows {
            if r.info.size >= *floor && r.info.size < ceil {
                n += 1;
                b += r.info.size;
            }
        }
        st.buckets.push((label, n, b));
    }
    st.top = sorted
        .iter()
        .take(5)
        .map(|r| {
            (
                r.info.filename.clone(),
                r.item.media_key.clone(),
                r.info.size,
                r.info.timestamp / 1000,
                r.item.is_video,
            )
        })
        .collect();
    st
}

// set an item's caption
pub fn set_item_description(s: &Session, dedup_key: &str, description: &str) -> bool {
    s.rpc(
        "AQNOFd",
        &serde_json::json!([null, description, dedup_key]),
    )
    .is_some()
}

// exiftool-style: caption embedded in the file ("other" field) → item caption
// pairs = (media_key, dedup_key) from the user's selection
pub fn copy_exif_description(s: &Session, pairs: &[(String, String)]) -> usize {
    let mut done = 0;
    for (media_key, dedup) in pairs {
        // the file's own embedded caption rides the ext object; refetch per item
        let v = s.rpc("fDcn4b", &serde_json::json!([media_key, 1, null, null, 1]));
        let Some(v) = v else { continue };
        let other = v[0][0][31].as_str().unwrap_or("");
        if other.is_empty() {
            continue;
        }
        // zero-width space so it differs from the raw "other" value
        if set_item_description(s, dedup, &format!("{}\u{200B}", other)) {
            done += 1;
        }
    }
    done
}

// date/time from filename (exiftool-style parse), applied via RPC
pub fn parse_date_from_filename(name: &str) -> Option<i64> {
    let digits: Vec<char> = name.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return None;
    }
    let s: String = digits.iter().collect();
    let year: i32 = s[0..4].parse().ok()?;
    if !(1970..2100).contains(&year) {
        return None;
    }
    let month: u32 = s[4..6].parse().ok()?;
    let day: u32 = s[6..8].parse().ok()?;
    let (mut hh, mut mm, mut ss) = (0u32, 0u32, 0u32);
    if digits.len() >= 14 {
        hh = s[8..10].parse().ok()?;
        mm = s[10..12].parse().ok()?;
        ss = s[12..14].parse().ok()?;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // use the same civil-day math as the date rail and storage report
    Some(
        days_from_civil(year as i64, month, day)
            .checked_mul(86400)?
            .checked_add((hh as i64) * 3600 + (mm as i64) * 60 + ss as i64)?,
    )
}

// date/time from filename (exiftool-style parse), applied via RPC
// pairs = (media_key, dedup_key) from the user's selection
pub fn set_timestamp_from_filename(s: &Session, pairs: &[(String, String)]) -> usize {
    let keys: Vec<String> = pairs.iter().map(|(m, _)| m.clone()).collect();
    let mut done = 0;
    for (media_key, dedup) in pairs {
        let Some(info) = bulk_info(s, &[media_key.clone()])
            .into_iter()
            .next()
            .filter(|i| i.tz_offset != 0 || i.timestamp != 0)
        else {
            continue;
        };
        if let Some(ts) = parse_date_from_filename(&info.filename) {
            if s.rpc(
                "DaSgWe",
                &serde_json::json!([[dedup, ts, info.tz_offset]]),
            )
            .is_some()
            {
                done += 1;
            }
        }
    }
    let _ = keys;
    done
}

// parse a YYYY-MM-DD string typed by the user into a unix timestamp (UTC day
// start). Returns None for malformed or out-of-range dates.
pub fn parse_ymd(s: &str) -> Option<i64> {
    let t = s.trim();
    let p: Vec<&str> = t.split('-').collect();
    if p.len() != 3 || p[0].len() != 4 || p[1].len() != 2 || p[2].len() != 2 {
        return None;
    }
    let y: i64 = p[0].parse().ok()?;
    let m: u32 = p[1].parse().ok()?;
    let d: u32 = p[2].parse().ok()?;
    if !(1970..=2100).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    // reject impossible dates such as 30 February by round-tripping
    if civil_from_days(days) != (y, m, d) {
        return None;
    }
    days.checked_mul(86400)
}

// set an explicit date on items: pairs = (media_key, dedup_key, "YYYY-MM-DD").
// Keeps each item's own timezone offset so Google shows the same calendar day.
pub fn set_timestamp_manual(s: &Session, pairs: &[(String, String, String)]) -> usize {
    let mut done = 0;
    for (media_key, dedup, text) in pairs {
        let Some(ts) = parse_ymd(text) else { continue };
        let tz = bulk_info(s, &[media_key.clone()])
            .into_iter()
            .next()
            .map(|i| i.tz_offset)
            .unwrap_or(0);
        if s.rpc("DaSgWe", &serde_json::json!([[dedup, ts, tz]])).is_some() {
            done += 1;
        }
    }
    done
}

// metadata CSV export
pub fn export_metadata(s: &Session, media_keys: &[String]) -> Option<String> {
    // the bulk-info RPC answers at most 35 keys per call; the full library
    // needs chunks or the CSV silently loses most of the rows
    let mut infos = Vec::new();
    for group in media_keys.chunks(35) {
        infos.extend(bulk_info(s, group));
    }
    if infos.is_empty() {
        return None;
    }
    let mut csv = String::from("filename,size_bytes,description,timestamp_iso,original_quality\n");
    for i in infos {
        let iso = if i.timestamp > 0 {
            // cheap unix→utc string
            let days = i.timestamp.div_euclid(86400);
            let secs = i.timestamp.rem_euclid(86400);
            let (y, m, d) = civil_from_days(days);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                y,
                m,
                d,
                secs / 3600,
                (secs % 3600) / 60,
                secs % 60
            )
        } else {
            String::new()
        };
        let fn_q = i.filename.replace('"', "\"\"");
        let ds_q = i.description.replace('"', "\"\"").replace('\n', " ");
        csv.push_str(&format!(
            "\"{}\",{},\"{}\",\"{}\",{}\n",
            fn_q,
            i.size,
            ds_q,
            iso,
            match i.original_quality {
                Some(true) => "yes",
                Some(false) => "saver",
                None => "?",
            }
        ));
    }
    let path = super::app_root().join("metadata-export.csv");
    std::fs::write(&path, csv).ok()?;
    Some(path.to_string_lossy().to_string())
}

// the other direction: a calendar day back to a unix day number
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// save items from a shared link / shared album into your own library
pub fn save_shared_to_library(s: &Session, link_id: &str, media_keys: &[String]) -> bool {
    s.rpc("V8RKJ", &serde_json::json!([media_keys, null, link_id]))
        .is_some()
}

// delete forever — NOT in the toolkit (Google exposes no verified RPC for it);
// best reverse-engineered guess at the XwAOJf action codes: 2 = permanent
pub fn delete_forever(s: &Session, dedup_keys: &[String]) -> bool {
    // the server's purge RPC only takes effect on something already trashed;
    // doing it here keeps the GUI buttons and --purge honest
    let _ = set_trashed(s, dedup_keys, true);
    s.rpc(
        "XwAOJf",
        &serde_json::json!([null, 2, dedup_keys, 2]),
    )
    .is_some()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_candidates_try_python3_python_then_windows_py() {
        for os_name in ["linux", "macos"] {
            let v = python_candidates(os_name);
            assert_eq!(v.iter().map(|(p, _)| *p).collect::<Vec<_>>(), vec!["python3", "python"]);
            assert!(v.iter().all(|(_, pre)| pre.is_empty()));
        }
        assert_eq!(
            python_candidates("windows"),
            vec![("python3", vec![]), ("python", vec![]), ("py", vec!["-3"])]
        );
    }

    #[test]
    fn wiz_extracts_quoted_value() {
        assert_eq!(wiz(r#"blah "at":"TOKEN123","x":1"#, "at"), Some("TOKEN123".into()));
        assert_eq!(wiz("no key here", "at"), None);
    }

    #[test]
    fn dechunk_reassembles_split_messages() {
        let a = r#"[["wrb.fr","abc","[1,2]"#;
        let b = r#",null,null,null,null]]"#;
        let body = format!(")]}}'\n{}\n{}\n{}\n{}", a.len(), a, b.len(), b);
        let msgs = dechunk(body);
        assert_eq!(msgs.len(), 1, "two chunks of one JSON value must join");
        assert!(msgs[0].contains("wrb.fr") && msgs[0].ends_with("]]"));
    }

    #[test]
    fn dechunk_rejects_oversized_length() {
        // the malformed length used to wrap and panic on the slice
        let body = ")]}'\n999999999999999999999999\n[]".to_string();
        assert!(dechunk(body).is_empty());
    }

    #[test]
    fn urlencode_is_strict_allowlist() {
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlencode("AZaz09-_.~"), "AZaz09-_.~");
    }

    #[test]
    fn parse_item_basic_and_video_flags() {
        let photo = serde_json::json!([
            "KEY1",
            ["https://img/abc", 1920, 1080],
            1556949153,
            "DEDUP1",
            {"72930366": [null, "holiday.jpg"], "163238866": [true]}
        ]);
        let p = parse_item(&photo).unwrap();
        assert_eq!(p.media_key, "KEY1");
        assert_eq!(p.dedup_key, "DEDUP1");
        assert_eq!(p.title, "holiday.jpg");
        assert!(!p.is_video && p.is_favorite);
        assert_eq!(p.ts, 1556949153);

        let video = serde_json::json!([
            "KEY2", ["https://img/def", 1280, 720], 1556949153000i64, "DEDUP2",
            {"76647426": [31000]}
        ]);
        let v = parse_item(&video).unwrap();
        assert!(v.is_video && v.duration_ms == 31000);
        assert_eq!(v.ts, 1556949153, "millisecond timestamps must be normalised");
        assert!(parse_item(&serde_json::json!(["ONLYKEY"])).is_none());
    }

    #[test]
    fn parse_generic_page_finds_rows_and_token() {
        let page = serde_json::json!([
            [["KEY1", ["t"], 1, "D1"], ["KEY2", ["t"], 2, "D2"]],
            "NEXTTOKEN1234567890"
        ]);
        let (items, next) = parse_generic_page(&page);
        assert_eq!(items.len(), 2);
        assert_eq!(next.as_deref(), Some("NEXTTOKEN1234567890"));
    }

    #[test]
    fn thousands_and_day_label() {
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(0), "0");
        // 0 and 1 Jan 1970 round-trip through the shared civil math
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(day_label(0), "-"); // non-positive timestamps are blank by design
    }

    #[test]
    fn curl_quote_drops_crlf_and_escapes() {
        assert_eq!(curl_quote("a\"b\\c\nd\te"), "\"a\\\"b\\\\cd\\te\"");
        assert!(!curl_quote("x\r\ny").contains('\n'));
    }

    fn fixture_session(jar: &str, email: &str) -> Session {
        Session {
            jar: jar.into(),
            email: email.into(),
            cookie: "SID=fake; HSID=fake".into(),
            at: "OLD_AT".into(),
            fsid: "fsid".into(),
            bl: "bl".into(),
            path: "/".into(),
            rapt: "".into(),
            fife_cookie: "OSID=fake".into(),
        }
    }

    #[test]
    fn session_key_is_jar_plus_email() {
        let a = session_key("brave|Default", "same@gmail.com");
        let b = session_key("brave|Profile 1", "same@gmail.com");
        assert_eq!(a, "brave|Default|same@gmail.com");
        assert_ne!(a, b, "same email in two profiles must be two cache entries");
    }

    #[test]
    fn cached_session_survives_offline_but_dies_on_signed_out_page() {
        let s = fixture_session("brave|Default", "me@gmail.com");
        // HTTP failure = offline: keep the session we already had
        assert_eq!(
            revalidate_cached(Some(s.clone()), None).map(|x| x.at),
            Some("OLD_AT".into())
        );
        // a fetched page that is definitely signed out discards it
        let signed_out = r#"<a href="https://accounts.google.com/ServiceLogin">Sign in</a>"#;
        assert!(revalidate_cached(Some(s.clone()), Some(signed_out.into())).is_none());
        // a signed-in page for the same email refreshes the at-token
        let signed_in = r#"{"oPEP7c":"me@gmail.com","SNlM0e":"NEW_AT"}"#;
        let ok = revalidate_cached(Some(s), Some(signed_in.into())).unwrap();
        assert_eq!(ok.at, "NEW_AT");
        assert_eq!(ok.email, "me@gmail.com");
    }

    // The shape Session::import_sessions writes for a Windows ABE export: all
    // RPC tokens empty until the first revalidation.
    fn imported_session_value() -> serde_json::Value {
        serde_json::json!({
            "jar": "imported|Default", "email": "you@gmail.com",
            "cookie": "SID=x", "fife_cookie": "OSID=imported",
            "at": "", "fsid": "", "bl": "", "path": "", "rapt": ""
        })
    }

    #[test]
    fn imported_session_produces_well_formed_rpc_url() {
        let s = Session::session_from_value(&imported_session_value()).unwrap();
        assert_eq!(s.path, "/", "empty imported path must fall back to root");
        let url = s.rpc_url("EzwWhf");
        assert!(
            url.starts_with("https://photos.google.com/data/batchexecute?"),
            "imported session must build a well-formed RPC URL"
        );
        assert!(!url.contains("google.comdata"));
    }

    #[test]
    fn revalidation_hydrates_all_imported_rpc_tokens_from_html() {
        let s = Session::session_from_value(&imported_session_value()).unwrap();
        let html = r#"{"oPEP7c":"you@gmail.com","SNlM0e":"AT","FdrFJe":"FS",
            "cfb2h":"BL","eptZe":"/_/photosui/","Dbw5Ud":"RA"}"#;
        let ok = revalidate_cached(Some(s), Some(html.into())).unwrap();
        assert_eq!(ok.at, "AT");
        assert_eq!(ok.fsid, "FS");
        assert_eq!(ok.bl, "BL");
        assert_eq!(ok.path, "/_/photosui/");
        assert_eq!(ok.rapt, "RA");
        assert!(ok
            .rpc_url("EzwWhf")
            .starts_with("https://photos.google.com/_/photosui/data/batchexecute?"));
        // a page missing the optional tokens must still leave a usable URL
        let sparse = r#"{"oPEP7c":"you@gmail.com","SNlM0e":"AT"}"#;
        let ok = revalidate_cached(
            Some(Session::session_from_value(&imported_session_value()).unwrap()),
            Some(sparse.into()),
        )
        .unwrap();
        assert_eq!(ok.path, "/");
        assert_eq!(ok.bl, DEFAULT_BL);
        assert!(ok.rpc_url("x").starts_with("https://photos.google.com/data/batchexecute?"));
    }

    #[test]
    fn resave_preserves_imported_fife_but_not_normal_capture_fife() {
        let mut fresh = serde_json::json!({"jar": "j"});
        let imported = serde_json::json!({"fife_cookie": "OSID=imported"});
        merge_stored_fife(&mut fresh, Some(&imported));
        assert_eq!(fresh["fife_cookie"], "OSID=imported");
        // a normal capture has no stored fife: do not freeze one into the cache
        let mut fresh = serde_json::json!({"jar": "j"});
        merge_stored_fife(&mut fresh, Some(&serde_json::json!({"fife_cookie": ""})));
        assert!(fresh.get("fife_cookie").is_none());
        merge_stored_fife(&mut fresh, None);
        assert!(fresh.get("fife_cookie").is_none());
    }

    #[test]
    fn legacy_capture_probes_one_authuser_per_jar_and_keeps_shell_fallback() {
        let sources = vec![("jarA".into(), "cookieA".into()), ("jarB".into(), "cookieB".into())];
        // Two signed-in jars at the SAME authuser: the first jar still wins.
        // The removed implementation used found[authuser] and picked jarB.
        let both = |url: &str, cookie: &str| -> Option<String> {
            assert!(url.contains("authuser=1"), "legacy probes the requested index");
            let email = if cookie == "cookieA" { "a@gmail.com" } else { "b@gmail.com" };
            Some(format!(r#"{{"oPEP7c":"{}","SNlM0e":"AT"}}"#, email))
        };
        let s = legacy_probe(&sources, 1, "FIFE", both).unwrap();
        assert_eq!(s.jar, "jarA");
        assert_eq!(s.email, "a@gmail.com");

        // A sign-in shell from jarA must be kept when no jar is signed in at
        // that index (old scrape's fallback, dropped by the removed version).
        let shells = |_url: &str, cookie: &str| -> Option<String> {
            if cookie == "cookieA" {
                Some(r#"{"SNlM0e":"AT_A"}"#.into())
            } else {
                None
            }
        };
        let s = legacy_probe(&sources, 1, "FIFE", shells).unwrap();
        assert_eq!(s.jar, "jarA");
        assert_eq!(s.at, "AT_A");
        assert_eq!(s.email, "", "a shell has no oPEP7c email");

        // A later signed-in jar beats an earlier shell.
        let mixed = |_url: &str, cookie: &str| -> Option<String> {
            match cookie {
                "cookieA" => Some(r#"{"SNlM0e":"AT_A"}"#.into()),
                "cookieB" => Some(r#"{"oPEP7c":"b@gmail.com","SNlM0e":"AT_B"}"#.into()),
                _ => None,
            }
        };
        let s = legacy_probe(&sources, 1, "FIFE", mixed).unwrap();
        assert_eq!(s.jar, "jarB");
        assert_eq!(s.at, "AT_B");

        // No token anywhere: never fabricate a session.
        assert!(legacy_probe(&sources, 1, "FIFE", |_, _| Some("<html>none</html>".into())).is_none());
    }

    #[test]
    fn registry_round_trip_in_temp_home() {
        // Path injection instead of mutating HOME: this test runs in parallel
        // with other tests that may read HOME/app_root().
        let dir = std::env::temp_dir().join(format!(
            "gphotos-app-t2-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("accounts.json");
        let rows = vec![
            AccountRecord {
                jar: "brave|Default".into(),
                email: "one@gmail.com".into(),
                name: "One".into(),
                hidden: true,
                last_seen: 100,
                last_ok: 90,
            },
            AccountRecord {
                jar: "brave|Profile 1".into(),
                email: "two@gmail.com".into(),
                name: "".into(),
                hidden: false,
                last_seen: 200,
                last_ok: 0,
            },
            AccountRecord {
                jar: "broken|jar".into(),
                email: "".into(), // invalid rows are ignored on load
                name: "Broken".into(),
                hidden: false,
                last_seen: 0,
                last_ok: 0,
            },
        ];
        assert!(save_registry_to(&p, &rows));
        assert_eq!(load_registry_from(&p), vec![rows[0].clone(), rows[1].clone()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&p).unwrap().mode();
            assert_eq!(mode & 0o077, 0, "registry must not be group/world readable");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_list_accounts_drops_dead_rows_and_reads_names() {
        // Fake fixture only: real ListAccounts payloads carry the XSSI prefix.
        let fixture = r#")]}'
[
  ["104234567890123456789", 1, "Alice Example", "alice@gmail.com", "https://lh3.googleusercontent.com/a/pic", null, null, null, null, 1, null, null, null, null, 0],
  ["104234567890123456790", 1, "Bob Example", "bob@gmail.com", "https://lh3.googleusercontent.com/a/pic2", null, null, null, null, 0, null, null, null, null, 0],
  ["104234567890123456791", 1, "Carol Example", "carol@gmail.com", "https://lh3.googleusercontent.com/a/pic3", null, null, null, null, 1, null, null, null, null, 1],
  ["104234567890123456792", "Fallback Name", null, "dave@gmail.com"]
]"#;
        let rows = parse_list_accounts(fixture);
        assert_eq!(rows.len(), 2, "both dead rows must be dropped, short row kept");
        assert_eq!(rows[0], ("alice@gmail.com".into(), "Alice Example".into(), true));
        assert_eq!(rows[1], ("dave@gmail.com".into(), "Fallback Name".into(), false),
            "short row has no flags: keep it, name falls back to row[1]");
    }
}
