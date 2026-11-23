#!/usr/bin/env python3
# Dump every local browser's Google session cookies as name=value lines.
# Firefox is plaintext; Chromium-family needs AES-CBC (v11 = 32-byte HMAC prefix).
import glob, hashlib, os, sqlite3, shutil, sys, tempfile

COOKIE_HOSTS = "('.google.com','photos.google.com','accounts.google.com')"
# Content-free on purpose: stderr is machine-facing, never a place for cookie values.
ABE_NOTICE = (
    "gphotos-cookie-helper: Windows Chromium 127+ cookie jars use App-Bound "
    "Encryption (ABE); their cookies cannot be decrypted externally. "
    "Use the local cookie-export fallback in docs/WINDOWS-SIGNIN.md."
)
# Same fixed prefix as ABE_NOTICE so Rust forwards it and nothing else. No
# cookie values, paths or key material ever go to stderr.
MACOS_KEYRING_NOTICE = (
    "gphotos-cookie-helper: automatic cookie decryption needs the Linux "
    "keyring; on macOS use the cookie-export import instead."
)
# jar browser label, keyring label, then the config-relative folder per OS.
# The relative path after each base is what forms the jar id, so existing ids
# keep their browser|profile|db shape on every platform.
CHROMIUM_BROWSERS = (
    ("brave", "Brave Safe Storage", "BraveSoftware",
     "BraveSoftware/Brave-Browser/User Data", "BraveSoftware/Brave-Browser"),
    ("chrome", "Chrome Safe Storage", "google-chrome",
     "Google/Chrome/User Data", "Google/Chrome"),
    ("chromium", "Chromium Safe Storage", "chromium",
     "Chromium/User Data", "Chromium"),
)

def platform_name():
    """Normalised OS name; kept separate so path/round tests need no real DBs."""
    if sys.platform.startswith("win"):
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return "linux"

def abe_notice(os_name=None):
    # We never try to defeat ABE: Windows jars protected by it cannot be read
    # from outside the browser process; the export-file fallback is the route.
    return ABE_NOTICE if (os_name or platform_name()) == "windows" else None

def macos_keyring_notice(os_name=None):
    # keyring_pw() uses Linux-only secretstorage (D-Bus): on macOS it returns
    # no password, so no jar decrypts and the 1003-round path never runs. Say
    # so once instead of silently yielding zero accounts on auto sign-in.
    return MACOS_KEYRING_NOTICE if (os_name or platform_name()) == "macos" else None

def pbkdf2_rounds(os_name=None):
    # Chromium wraps the macOS keyring password in 1003 PBKDF2 rounds; the
    # Linux keyring and Windows DPAPI paths use the historical single round.
    return 1003 if (os_name or platform_name()) == "macos" else 1

def chromium_key(pw, os_name=None):
    return hashlib.pbkdf2_hmac("sha1", pw, b"saltysalt", pbkdf2_rounds(os_name), 16)

def emit(pairs):
    for n, v in pairs:
        if n and v and all(32 <= ord(c) < 127 for c in v):
            print(f"{n}={v}")

def keyring_pw(label):
    # the Default Keyring can be locked: its get_all_items() raises, which used
    # to abort the whole lookup and lose the unlocked items in other collections
    try:
        import secretstorage
        bus = secretstorage.dbus_init()
    except Exception:
        return b""
    for coll in secretstorage.get_all_collections(bus):
        try:
            items = coll.get_all_items()
        except Exception:
            continue
        for item in items:
            try:
                if item.get_label() != label:
                    continue
                if item.is_locked():
                    try:
                        item.unlock()
                    except Exception:
                        pass
                secret = item.get_secret()
                if secret:
                    return secret
            except Exception:
                continue
    return b""

def cookie_sources(home=None, os_name=None, env=None):
    """Yield (jar_id, db_path, key_label) for every browser profile.

    jar_id is browser-kind|profile-dir|cookie-db-relative-path, built from paths
    relative to the browser's config root so no absolute path leaks into ids.
    key_label None marks Firefox's plaintext cookie store. os_name/env are test
    seams; production values come from platform_name() and os.environ.
    """
    os_name = os_name or platform_name()
    root = os.path.expanduser("~") if home is None else home
    env = os.environ if env is None else env
    if os_name == "windows":
        local = env.get("LOCALAPPDATA") or os.path.join(root, "AppData", "Local")
        appdata = env.get("APPDATA") or os.path.join(root, "AppData", "Roaming")
        chromium_root, firefox_root = local, os.path.join(appdata, "Mozilla", "Firefox", "Profiles")
    elif os_name == "macos":
        support = os.path.join(root, "Library", "Application Support")
        chromium_root, firefox_root = support, os.path.join(support, "Firefox", "Profiles")
    else:
        chromium_root, firefox_root = os.path.join(root, ".config"), os.path.join(root, ".mozilla/firefox")

    sources = []
    for kind, key, linux, windows, mac in CHROMIUM_BROWSERS:
        folder = linux if os_name == "linux" else windows if os_name == "windows" else mac
        base = os.path.join(chromium_root, *folder.split("/"))
        # On Linux Brave nests an install dir (Brave-Origin / Brave-Browser)
        # above profiles; keep it in the id so existing identities are stable.
        profile_glob = "*/*" if (kind == "brave" and os_name == "linux") else "*"
        dbs = sorted(set(
            glob.glob(os.path.join(base, profile_glob, "Cookies")) +
            glob.glob(os.path.join(base, profile_glob, "Network", "Cookies"))
        ))
        for db in dbs:
            parts = os.path.relpath(db, base).split(os.sep)
            if len(parts) >= 2 and parts[-2] == "Network":
                cookie_rel, profile = "/".join(parts[-2:]), parts[:-2]
            else:
                cookie_rel, profile = parts[-1], parts[:-1]
            if not profile:
                continue  # DB at the config root would yield a degenerate id like chrome||Cookies
            sources.append((f"{kind}|{'/'.join(profile)}|{cookie_rel}", db, key))

    for db in sorted(glob.glob(os.path.join(firefox_root, "*", "cookies.sqlite"))):
        profile = os.path.basename(os.path.dirname(db))
        sources.append((f"firefox|{profile}|cookies.sqlite", db, None))
    return sources

def firefox_rows(db):
    fd, t = tempfile.mkstemp(suffix=".sqlite")
    try:
        # copyfileobj into the fd keeps mkstemp's 0600 mode (shutil.copy
        # would copy the source's permissions onto the temp file)
        with os.fdopen(fd, "wb") as dst, open(db, "rb") as src:
            shutil.copyfileobj(src, dst)
        con = sqlite3.connect(t)
        try:
            return con.execute(
                "SELECT name, value FROM moz_cookies WHERE host IN " + COOKIE_HOSTS
            ).fetchall()
        finally:
            con.close()
    except Exception:
        return []
    finally:
        os.remove(t)

def chromium_rows(db, key_label):
    if not os.path.isfile(db):
        return []
    pw = keyring_pw(key_label)
    if not pw:
        return []
    key = chromium_key(pw)
    from Crypto.Cipher import AES
    from Crypto.Util.Padding import unpad
    fd, t = tempfile.mkstemp(suffix="c")
    try:
        with os.fdopen(fd, "wb") as dst, open(db, "rb") as src:
            shutil.copyfileobj(src, dst)
        con = sqlite3.connect(t)
        try:
            rows = con.execute(
                "SELECT name, encrypted_value FROM cookies WHERE host_key IN " + COOKIE_HOSTS
            ).fetchall()
        finally:
            con.close()
        out = []
        for n, ev in rows:
            if not ev or ev[:3] not in (b"v10", b"v11"):
                continue
            try:
                d = unpad(AES.new(key, AES.MODE_CBC, b" " * 16).decrypt(ev[3:]), 16)
                # newer chromium prepends a 32-byte domain-integrity HMAC
                if not all(32 <= b < 127 for b in d[:32]):
                    d = d[32:]
                s = d.decode("utf-8", errors="replace")
                if s and all(32 <= ord(c) < 127 for c in s):
                    out.append((n, s))
            except Exception:
                pass
        return out
    except Exception:
        return []
    finally:
        os.remove(t)

def main(home=None, os_name=None, env=None, stderr=None):
    os_name = os_name or platform_name()
    err = sys.stderr if stderr is None else stderr
    warned = False
    for jar, db, key_label in cookie_sources(home, os_name=os_name, env=env):
        if key_label is None:
            rows = firefox_rows(db)
        else:
            notice = abe_notice(os_name) or macos_keyring_notice(os_name)
            if notice and not warned:
                warned = True
                print(notice, file=err)
            rows = chromium_rows(db, key_label)
        if rows:
            print(f"# === {jar}")
            emit(rows)
    # NOTE: no browser_cookie3 fallback — it queries KWallet and summons
    # wallet-creation dialogs; our own chromium_rows covers those browsers

if __name__ == "__main__":
    main()
