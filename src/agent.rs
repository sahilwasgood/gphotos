// Machine-readable CLI for AI agents. One JSON object per line on stdout,
// errors on stderr, exit 0 on success / 1 on failure. No new dependency:
// output is built with serde_json::json! from library.rs public structs.
//
// Read commands are always available. Write commands require either the
// GPHOTOS_AGENT_WRITE=1 env var or --allow-write, so a read-only agent can
// never mutate the library by accident.

use crate::library;
use serde_json::{json, Value};

fn ok(command: &str, data: Value) -> i32 {
    println!("{}", json!({"ok": true, "command": command, "data": data}));
    0
}

fn err(command: &str, message: &str) -> i32 {
    eprintln!("{}", json!({"ok": false, "command": command, "error": message}));
    1
}

fn item_json(i: &library::MediaItem) -> Value {
    json!({
        "media_key": i.media_key,
        "dedup_key": i.dedup_key,
        "title": i.title,
        "ts": i.ts,
        "is_video": i.is_video,
        "is_favorite": i.is_favorite,
        "w": i.w,
        "h": i.h,
        "duration_ms": i.duration_ms,
    })
}

fn items_json(items: &[library::MediaItem], next: Option<String>) -> Value {
    json!({
        "items": items.iter().map(item_json).collect::<Vec<_>>(),
        "count": items.len(),
        "next_page_token": next,
    })
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|p| args.get(p + 1))
        .filter(|v| !v.starts_with('-'))
        .cloned()
}

fn keys(args: &[String]) -> Vec<String> {
    arg(args, "--keys")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn limit(args: &[String]) -> Option<usize> {
    arg(args, "--limit").and_then(|v| v.parse().ok())
}

fn dedup_keys(s: &library::Session, media_keys: &[String]) -> Vec<String> {
    // The UI keeps dedup keys in lib_items; headless has no App state, so
    // resolve from a list scan and fall back to the media key itself.
    let mut all: Vec<library::MediaItem> = Vec::new();
    for view in 1..=4u8 {
        let page = match view {
            1 => library::recent(s, None),
            2 => library::archive(s, None),
            3 => library::favorites(s, None),
            _ => library::trash(s, None),
        };
        all.extend(page.0);
    }
    media_keys.iter().map(|k| {
        all.iter().find(|i| i.media_key == *k)
            .map(|i| i.dedup_key.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| k.clone())
    }).collect()
}

fn write_allowed(args: &[String]) -> bool {
    std::env::var("GPHOTOS_AGENT_WRITE").map(|v| v == "1").unwrap_or(false)
        || args.iter().any(|a| a == "--allow-write")
}

fn no_session(command: &str) -> i32 {
    err(command, "no Google Photos session — sign in first")
}

/// Dispatch an --agent command. Returns None when argv does not request it.
pub fn run(argv: &[String]) -> Option<i32> {
    let p = argv.iter().position(|a| a == "--agent")?;
    let command = argv.get(p + 1).map(String::as_str).unwrap_or("help");

    if command == "help" || command == "--help" || command == "-h" {
        println!("{}", json!({
            "ok": true,
            "command": "help",
            "data": {
                "commands": [
                    "status", "list", "find", "info", "quota", "albums", "things", "links",
                    "favorite", "trash", "restore", "archive", "unarchive", "lock", "unlock",
                    "create-album", "share", "delete-forever"
                ],
                "write_gate": "set GPHOTOS_AGENT_WRITE=1 or pass --allow-write",
                "note": "read commands are always safe; write commands mutate your Google Photos account",
            }
        }));
        return Some(0);
    }

    let s = match library::Session::capture() {
        Some(s) => s,
        None => return Some(no_session(command)),
    };

    let code = match command {
        "status" => {
            let reg = library::load_registry();
            ok(command, json!({
                "app": "gphotos",
                "version": env!("CARGO_PKG_VERSION"),
                "account": s.email,
                "session": "ok",
                "accounts": reg.iter().map(|a| json!({"jar": a.jar, "email": a.email, "hidden": a.hidden})).collect::<Vec<_>>(),
                "write_enabled": write_allowed(argv),
            }))
        }
        "list" => {
            let view = arg(argv, "--view").unwrap_or_else(|| "recent".into());
            let page = arg(argv, "--page");
            let (items, next) = match view.as_str() {
                "recent" => library::recent(&s, page.as_deref()),
                "archive" => library::archive(&s, page.as_deref()),
                "favorites" => library::favorites(&s, page.as_deref()),
                "trash" => library::trash(&s, page.as_deref()),
                "locked" => library::locked(&s, page.as_deref()),
                _ => return Some(err(command, &format!("unknown view: {view}"))),
            };
            if library::last_rpc_failed() { return Some(err(command, "Google Photos did not answer — retry")); }
            let (items, next) = match limit(argv) {
                Some(n) => (items.into_iter().take(n).collect::<Vec<_>>(), next),
                None => (items, next),
            };
            ok(command, json!({"view": view, "data": items_json(&items, next)}))
        }
        "find" => {
            let q = match arg(argv, "--query") {
                Some(q) => q,
                None => return Some(err(command, "missing --query")),
            };
            let (items, next) = library::search(&s, &q, arg(argv, "--page").as_deref());
            if library::last_rpc_failed() { return Some(err(command, "Google Photos did not answer — retry")); }
            ok(command, json!({"query": q, "data": items_json(&items, next)}))
        }
        "info" => {
            let k = keys(argv);
            if k.is_empty() { return Some(err(command, "missing --keys")); }
            let infos = library::bulk_info(&s, &k);
            if library::last_rpc_failed() { return Some(err(command, "Google Photos did not answer — retry")); }
            ok(command, json!({"items": infos.iter().map(|b| json!({
                "media_key": b.media_key, "filename": b.filename, "description": b.description,
                "size": b.size, "ts": b.timestamp, "takes_space": b.takes_up_space,
                "original_quality": b.original_quality, "source": b.source,
            })).collect::<Vec<_>>()}))
        }
        "quota" => match library::quota(&s) {
            Some(q) => ok(command, json!({"used": q.used, "available": q.available, "gphotos": q.gphotos})),
            None => err(command, "could not read quota"),
        },
        "albums" => {
            let (albums, next) = library::albums(&s, arg(argv, "--page").as_deref());
            if library::last_rpc_failed() { return Some(err(command, "Google Photos did not answer — retry")); }
            ok(command, json!({"albums": albums.iter().map(|a| json!({
                "key": a.key, "title": a.title, "count": a.count, "is_shared": a.is_shared,
            })).collect::<Vec<_>>(), "next_page_token": next}))
        }
        "things" => {
            let t = library::things(&s);
            if library::last_rpc_failed() { return Some(err(command, "Google Photos did not answer — retry")); }
            ok(command, json!({"groups": t.iter().map(|x| json!({
                "name": x.name, "is_place": x.is_place, "entity_id": x.entity_id,
            })).collect::<Vec<_>>()}))
        }
        "links" => {
            let (links, next) = library::links(&s, arg(argv, "--page").as_deref());
            if library::last_rpc_failed() { return Some(err(command, "Google Photos did not answer — retry")); }
            ok(command, json!({"links": links.iter().map(|l| json!({"key": l.key, "count": l.count})).collect::<Vec<_>>(), "next_page_token": next}))
        }
        "favorite" | "unfavorite" => {
            if !write_allowed(argv) { return Some(err(command, "write disabled — set GPHOTOS_AGENT_WRITE=1 or pass --allow-write")); }
            let k = keys(argv);
            if k.is_empty() { return Some(err(command, "missing --keys")); }
            let dk = dedup_keys(&s, &k);
            let on = command == "favorite";
            let changed = library::set_favorite(&s, &dk, on);
            if changed { ok(command, json!({"changed": dk.len(), "favorite": on})) }
            else { err(command, "favorite RPC failed") }
        }
        "trash" | "restore" | "archive" | "unarchive" | "lock" | "unlock" => {
            if !write_allowed(argv) { return Some(err(command, "write disabled — set GPHOTOS_AGENT_WRITE=1 or pass --allow-write")); }
            let k = keys(argv);
            if k.is_empty() { return Some(err(command, "missing --keys")); }
            let dk = dedup_keys(&s, &k);
            let done = match command {
                "trash" => library::set_trashed(&s, &dk, true),
                "restore" => library::set_trashed(&s, &dk, false),
                "archive" => library::set_archived(&s, &dk, true),
                "unarchive" => library::set_archived(&s, &dk, false),
                "lock" => library::lock_items(&s, &dk),
                "unlock" => library::unlock_items(&s, &dk),
                _ => false,
            };
            if done { ok(command, json!({"changed": dk.len()})) } else { err(command, "RPC failed") }
        }
        "create-album" => {
            if !write_allowed(argv) { return Some(err(command, "write disabled — set GPHOTOS_AGENT_WRITE=1 or pass --allow-write")); }
            let name = match arg(argv, "--name") { Some(n) => n, None => return Some(err(command, "missing --name")) };
            match library::create_album(&s, &name) {
                Some(key) => ok(command, json!({"album_key": key, "title": name})),
                None => err(command, "create album failed"),
            }
        }
        "share" => {
            if !write_allowed(argv) { return Some(err(command, "write disabled — set GPHOTOS_AGENT_WRITE=1 or pass --allow-write")); }
            let k = keys(argv);
            if k.is_empty() { return Some(err(command, "missing --keys")); }
            match library::create_share_link(&s, &k) {
                Some(url) => ok(command, json!({"url": url})),
                None => err(command, "share link failed"),
            }
        }
        "delete-forever" => {
            if !write_allowed(argv) { return Some(err(command, "write disabled — set GPHOTOS_AGENT_WRITE=1 or pass --allow-write")); }
            let k = keys(argv);
            if k.is_empty() { return Some(err(command, "missing --keys")); }
            // explicit confirm token must match the single media_key, mirroring
            // the UI's two-step warning for a destructive, unrecoverable action
            let confirm = arg(argv, "--confirm");
            if k.len() != 1 || confirm.as_deref() != Some(k[0].as_str()) {
                return Some(err(command, "delete-forever needs exactly one --keys entry and --confirm <that same media_key>"));
            }
            let dk = dedup_keys(&s, &k);
            let ok1 = library::set_trashed(&s, &dk, true);
            let ok2 = library::delete_forever(&s, &dk);
            if ok1 && ok2 { ok(command, json!({"deleted": 1, "media_key": k[0]})) }
            else { err(command, "delete failed") }
        }
        other => err("agent", &format!("unknown command: {other}; run --agent help")),
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_keys_splits_comma_list() {
        let argv: Vec<String> = ["--agent", "favorite", "--keys", " k1, k2 ,,", "--allow-write"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(keys(&argv), vec!["k1", "k2"]);
        assert!(write_allowed(&argv));
    }

    #[test]
    fn delete_forever_confirm_requires_exact_key() {
        // The guard is a pure comparison of --confirm to the single key.
        let argv: Vec<String> = ["--agent", "delete-forever", "--keys", "K1", "--confirm", "K1"]
            .iter().map(|s| s.to_string()).collect();
        let k = keys(&argv);
        assert_eq!(k.len(), 1);
        assert_eq!(arg(&argv, "--confirm").as_deref(), Some(k[0].as_str()));
        let bad: Vec<String> = ["--agent", "delete-forever", "--keys", "K1", "--confirm", "other"]
            .iter().map(|s| s.to_string()).collect();
        assert_ne!(arg(&bad, "--confirm").as_deref(), Some(keys(&bad)[0].as_str()));
    }

    #[test]
    fn write_gate_defaults_to_off() {
        let argv: Vec<String> = ["--agent", "trash", "--keys", "K1"].iter().map(|s| s.to_string()).collect();
        // env may be set in CI; test the flag half only when env is unset
        if std::env::var("GPHOTOS_AGENT_WRITE").is_err() {
            assert!(!write_allowed(&argv));
        }
    }
}
