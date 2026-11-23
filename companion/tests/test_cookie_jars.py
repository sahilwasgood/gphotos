"""Fixture-only tests for per-profile cookie jars.

Never invokes either helper against real browser databases: cookie DB paths are
fixtures in a temp HOME and row extraction is patched, so no live cookie value
can reach stdout or this test process.
"""
import contextlib
import hashlib
import io
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HELPERS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, HELPERS_DIR)
# Import-time output is the exact security failure this suite guards against:
# capture it at collection time so a regression can never print real cookies
# into the test run even before an assertion executes.
_import_out = io.StringIO()
with contextlib.redirect_stdout(_import_out):
    import gcookies
    import fifecookies
assert not _import_out.getvalue(), "importing the helpers must not print cookies"


class CookieJarTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = self.tmp.name

    def fixture_db(self, *parts):
        path = os.path.join(self.home, *parts)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        open(path, "wb").close()
        return path

    def brave_pair(self):
        return (
            self.fixture_db(".config", "BraveSoftware", "Brave-Origin", "Default", "Cookies"),
            self.fixture_db(".config", "BraveSoftware", "Brave-Origin", "Profile 1", "Network", "Cookies"),
        )

    def run_main(self, module):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            module.main(home=self.home)
        return out.getvalue()

    @staticmethod
    def sections(text):
        result, name = {}, None
        for line in text.splitlines():
            if line.startswith("# === "):
                name = line[6:].strip()
                result.setdefault(name, [])
            elif "=" in line and name:
                result[name].append(line)
        return result

    def test_two_brave_profiles_get_distinct_jar_ids_and_no_merged_cookies(self):
        default, profile1 = self.brave_pair()

        def rows(db, key_label):
            if db == default:
                return [("SID", "sid-default"), ("NID", "nid-default")]
            if db == profile1:
                return [("SID", "sid-profile1"), ("NID", "nid-profile1")]
            return []

        with mock.patch.object(gcookies, "chromium_rows", rows, create=True), \
             mock.patch.object(gcookies, "firefox_rows", lambda db: [], create=True):
            sections = self.sections(self.run_main(gcookies))

        self.assertEqual(set(sections), {
            "brave|Brave-Origin/Default|Cookies",
            "brave|Brave-Origin/Profile 1|Network/Cookies",
        })
        # Same cookie name in both jars: both values survive, neither clobbers the other.
        self.assertEqual(sections["brave|Brave-Origin/Default|Cookies"],
                         ["SID=sid-default", "NID=nid-default"])
        self.assertEqual(sections["brave|Brave-Origin/Profile 1|Network/Cookies"],
                         ["SID=sid-profile1", "NID=nid-profile1"])

    def test_enumerates_chrome_chromium_firefox_with_stable_relative_ids(self):
        self.fixture_db(".config", "google-chrome", "Profile 1", "Network", "Cookies")
        self.fixture_db(".config", "chromium", "Default", "Cookies")
        self.fixture_db(".mozilla", "firefox", "abc.default-release", "cookies.sqlite")

        ids = [jar for jar, _, _ in gcookies.cookie_sources(self.home)]
        self.assertIn("chrome|Profile 1|Network/Cookies", ids)
        self.assertIn("chromium|Default|Cookies", ids)
        self.assertIn("firefox|abc.default-release|cookies.sqlite", ids)
        for jar in ids:
            self.assertEqual(jar.count("|"), 2)
            self.assertNotIn(self.home, jar)

    def test_skips_db_at_config_root(self):
        # A Cookies file directly at the config root has no profile component;
        # it must not produce a degenerate id such as chrome||Cookies.
        base = os.path.join(self.home, ".config", "google-chrome")
        os.makedirs(base, exist_ok=True)
        root_db = os.path.join(base, "Cookies")
        open(root_db, "wb").close()

        def fake_glob(pattern, *args, **kwargs):
            return [root_db] if pattern == os.path.join(base, "*", "Cookies") else []

        with mock.patch.object(gcookies.glob, "glob", fake_glob):
            ids = [jar for jar, _, _ in gcookies.cookie_sources(self.home)]
        self.assertEqual(ids, [])

    def test_fifecookies_reads_every_profile_not_just_the_first(self):
        default, profile1 = self.brave_pair()

        def dump(db, label):
            if db == profile1:
                return [("OSID", "osid-p1", ".photos.fife.usercontent.google.com")]
            return [("SID", "sid-default", ".google.com")]

        sources = [
            ("brave|Brave-Origin/Default|Cookies", default, "Brave Safe Storage"),
            ("brave|Brave-Origin/Profile 1|Network/Cookies", profile1, "Brave Safe Storage"),
        ]
        with mock.patch.object(gcookies, "cookie_sources", lambda home=None, **kw: sources, create=True), \
             mock.patch.object(fifecookies, "dump", dump):
            text = self.run_main(fifecookies)

        self.assertIn("SID=sid-default", text)
        self.assertIn("OSID=osid-p1", text)
        self.assertNotIn("# ===", text)  # fife header is flat, Rust does not parse sections

    def test_importing_fifecookies_prints_nothing(self):
        # Fresh-process guard: importing must never decrypt/print real cookies.
        # capture_output keeps any regression off the console, in memory only.
        result = subprocess.run(
            [sys.executable, "-c", "import fifecookies"],
            cwd=HELPERS_DIR,
            capture_output=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.assertEqual(result.stdout, b"")

    def test_fife_header_merges_by_name_with_fife_and_last_jar_winning(self):
        # Rust cannot parse per-jar sections in the fife header, so names collide
        # across jars: pin the chosen merge (fife domain > plain google, later jar
        # > earlier jar) so it cannot silently change.
        db1, db2 = self.brave_pair()

        def dump(db, label):
            if db == db1:
                return [("SID", "sid-jar1", ".google.com"),
                        ("SHARED", "base-shared", ".google.com")]
            return [("SID", "sid-jar2", ".google.com"),
                    ("SHARED", "fife-shared", ".photos.fife.usercontent.google.com")]

        sources = [
            ("brave|Brave-Origin/Default|Cookies", db1, "Brave Safe Storage"),
            ("brave|Brave-Origin/Profile 1|Network/Cookies", db2, "Brave Safe Storage"),
        ]
        with mock.patch.object(gcookies, "cookie_sources", lambda home=None, **kw: sources, create=True), \
             mock.patch.object(fifecookies, "dump", dump):
            text = self.run_main(fifecookies)

        self.assertEqual(text.splitlines(), ["SID=sid-jar2", "SHARED=fife-shared"])

    def test_windows_paths_use_localappdata_and_appdata(self):
        local = os.path.join(self.home, "Local")
        roaming = os.path.join(self.home, "Roaming")
        self.fixture_db("Local", "Google", "Chrome", "User Data", "Default", "Network", "Cookies")
        self.fixture_db("Local", "BraveSoftware", "Brave-Browser", "User Data", "Profile 1", "Cookies")
        self.fixture_db("Roaming", "Mozilla", "Firefox", "Profiles", "abc.default", "cookies.sqlite")
        env = {"LOCALAPPDATA": local, "APPDATA": roaming}
        ids = [jar for jar, _, _ in gcookies.cookie_sources(self.home, os_name="windows", env=env)]
        self.assertEqual(ids, [
            "brave|Profile 1|Cookies",
            "chrome|Default|Network/Cookies",
            "firefox|abc.default|cookies.sqlite",
        ])

    def test_windows_paths_fall_back_to_home_appdata(self):
        self.fixture_db("AppData", "Local", "Google", "Chrome", "User Data", "Default", "Cookies")
        self.fixture_db("AppData", "Roaming", "Mozilla", "Firefox", "Profiles", "p", "cookies.sqlite")
        ids = [jar for jar, _, _ in gcookies.cookie_sources(self.home, os_name="windows", env={})]
        self.assertEqual(ids, ["chrome|Default|Cookies", "firefox|p|cookies.sqlite"])

    def test_macos_paths_use_application_support(self):
        support = "Library/Application Support"
        self.fixture_db(support, "Google", "Chrome", "Default", "Cookies")
        self.fixture_db(support, "Chromium", "Default", "Network", "Cookies")
        self.fixture_db(support, "Firefox", "Profiles", "abc.default", "cookies.sqlite")
        ids = [jar for jar, _, _ in gcookies.cookie_sources(self.home, os_name="macos")]
        self.assertEqual(ids, [
            "chrome|Default|Cookies",
            "chromium|Default|Network/Cookies",
            "firefox|abc.default|cookies.sqlite",
        ])

    def test_pbkdf2_rounds_are_macos_only(self):
        self.assertEqual(gcookies.pbkdf2_rounds("linux"), 1)
        self.assertEqual(gcookies.pbkdf2_rounds("windows"), 1)
        self.assertEqual(gcookies.pbkdf2_rounds("macos"), 1003)
        self.assertEqual(gcookies.pbkdf2_rounds(), gcookies.pbkdf2_rounds(gcookies.platform_name()))
        self.assertEqual(gcookies.chromium_key(b"pw", "linux"),
                         hashlib.pbkdf2_hmac("sha1", b"pw", b"saltysalt", 1, 16))
        self.assertEqual(gcookies.chromium_key(b"pw", "macos"),
                         hashlib.pbkdf2_hmac("sha1", b"pw", b"saltysalt", 1003, 16))

    def test_windows_abe_notice_is_content_free_and_stderr_only(self):
        notice = gcookies.abe_notice("windows")
        self.assertIn("App-Bound Encryption", notice)
        self.assertIn("docs/WINDOWS-SIGNIN.md", notice)
        self.assertIsNone(gcookies.abe_notice("linux"))
        self.assertIsNone(gcookies.abe_notice("macos"))
        self.fixture_db("AppData", "Local", "Google", "Chrome", "User Data", "Default", "Network", "Cookies")
        err, out = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(out), mock.patch.object(
                gcookies, "chromium_rows", lambda db, label: [], create=True):
            gcookies.main(home=self.home, os_name="windows", env={}, stderr=err)
        self.assertIn("App-Bound Encryption", err.getvalue())
        self.assertNotIn("SID=", err.getvalue())
        self.assertNotIn("App-Bound Encryption", out.getvalue())
        # Linux stays silent even with a Chromium jar present
        linux_err = io.StringIO()
        with contextlib.redirect_stderr(linux_err):
            with contextlib.redirect_stdout(io.StringIO()):
                gcookies.main(home=self.home, os_name="linux", env={}, stderr=linux_err)
        self.assertEqual(linux_err.getvalue(), "")

    def test_macos_keyring_notice_is_content_free_and_stderr_only(self):
        # No real keyring is touched: chromium_rows is patched, and a fixture
        # Chromium jar is the only thing that can trigger the notice.
        notice = gcookies.macos_keyring_notice("macos")
        self.assertIn("Linux keyring", notice)
        self.assertIn("cookie-export import", notice)
        self.assertTrue(notice.startswith("gphotos-cookie-helper:"))
        self.assertIsNone(gcookies.macos_keyring_notice("linux"))
        self.assertIsNone(gcookies.macos_keyring_notice("windows"))
        support = "Library/Application Support"
        self.fixture_db(support, "Google", "Chrome", "Default", "Cookies")
        err, out = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), mock.patch.object(
                gcookies, "chromium_rows", lambda db, label: [], create=True):
            gcookies.main(home=self.home, os_name="macos", stderr=err)
        self.assertIn("Linux keyring", err.getvalue())
        self.assertNotIn("SID=", err.getvalue())
        self.assertNotIn("Linux keyring", out.getvalue())
        # No notice without a Chromium jar: the ABE notice follows jar presence
        empty_err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()):
            gcookies.main(home=self.tmp.name + "-empty", os_name="macos", stderr=empty_err)
        self.assertEqual(empty_err.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
