#!/usr/bin/env python3
# Cookie header for the fife image cdn: the .google.com session cookies plus
# the fife-scoped OSID pair, which gcookies.py's .google.com host filter never
# emits — without those the cdn answers 403/placeholder.
import os, shutil, sqlite3, sys, tempfile
import gcookies

def keyring_pw(label):
    # a locked collection raises on get_all_items(); skip it instead of
    # aborting the lookup and losing items from unlocked collections
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

def dump(db, label):
    if not os.path.isfile(db):
        return []
    pw = keyring_pw(label)
    if not pw:
        return []
    key = gcookies.chromium_key(pw)
    from Crypto.Cipher import AES
    from Crypto.Util.Padding import unpad
    fd, t = tempfile.mkstemp(suffix="c")
    try:
        with os.fdopen(fd, "wb") as dst, open(db, "rb") as src:
            shutil.copyfileobj(src, dst)
        con = sqlite3.connect(t)
        try:
            rows = con.execute(
                "SELECT name, encrypted_value, host_key FROM cookies "
                "WHERE host_key LIKE '%google.com'"
            ).fetchall()
        finally:
            con.close()
        out = []
        for n, ev, host in rows:
            if not ev or ev[:3] not in (b"v10", b"v11"):
                continue
            try:
                d = unpad(AES.new(key, AES.MODE_CBC, b" " * 16).decrypt(ev[3:]), 16)
                # newer chromium prepends a 32-byte domain-integrity HMAC
                if not all(32 <= b < 127 for b in d[:32]):
                    d = d[32:]
                s = d.decode("utf-8", errors="replace")
                if s and all(32 <= ord(c) < 127 for c in s):
                    dom = host if host.startswith(".") else "." + host
                    out.append((n, s, dom))
            except Exception:
                pass
        return out
    finally:
        os.remove(t)

def main(home=None, os_name=None, env=None, stderr=None):
    # The fife header is flat (Rust does not parse sections), so gather every
    # Chromium-family profile instead of stopping at the first one that opens.
    os_name = os_name or gcookies.platform_name()
    err = sys.stderr if stderr is None else stderr
    base, fife = {}, {}
    warned = False
    sources = gcookies.cookie_sources(home, os_name=os_name, env=env)
    for jar, db, key_label in sources:
        if key_label is None:
            continue  # Firefox plaintext DBs have no encrypted OSID pair
        notice = gcookies.abe_notice(os_name)
        if notice and not warned:
            warned = True
            print(notice, file=err)
        for n, v, dom in dump(db, key_label):
            (fife if "photos.fife.usercontent.google.com" in dom else base)[n] = v
    merged = {**base, **fife}
    for n, v in merged.items():
        print(f"{n}={v}")

if __name__ == "__main__":
    main()
