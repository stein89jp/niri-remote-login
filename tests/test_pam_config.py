"""Check the dedicated niri-rdp PAM policy without touching host PAM files."""
from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PAM_FILE = ROOT / "broker/config/niri-rdp.pam"
FAILLOCK_ARGS = ["dir=/run/faillock-niri-rdp", "deny=5", "unlock_time=300"]
GATE_ARGS = ["uid", ">=", "1000", "user", "ingroup", "niri-rdp-users", "quiet"]


def parse_pam(text):
    """Return (type, optional, control, module, args) for each rule line."""
    rules = []
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        kind, rest = line.split(None, 1)
        optional = kind.startswith("-")
        kind = kind.lstrip("-")
        if rest.startswith("["):
            end = rest.index("]")
            control, rest = rest[:end + 1], rest[end + 1:]
        else:
            control, rest = rest.split(None, 1)
        fields = rest.split()
        rules.append((kind, optional, control, fields[0], fields[1:]))
    return rules


def stack(rules, kind):
    return [rule for rule in rules if rule[0] == kind]


class PamPolicyTests(unittest.TestCase):
    def setUp(self):
        self.rules = parse_pam(PAM_FILE.read_text())

    def test_policy_is_self_contained(self):
        # The inherited system-remote-login faillock locked console and sudo
        # after three RDP failures; nothing may pull it back in.
        for rule in self.rules:
            self.assertNotIn(rule[2], ("include", "substack"), rule)
            self.assertTrue(rule[3].endswith(".so"), rule)
            for name in ("system-remote-login", "system-login", "system-auth"):
                self.assertNotIn(name, " ".join(rule[3:4] + tuple(rule[4])), rule)
        self.assertEqual({rule[0] for rule in self.rules}, {"auth", "account", "session"})

    def test_group_gate_is_first_and_does_not_short_circuit(self):
        first = stack(self.rules, "auth")[0]
        self.assertEqual(first, ("auth", False, "required", "pam_succeed_if.so", GATE_ARGS))
        self.assertEqual(self.rules[0], first)
        for rule in self.rules:
            if rule[3] == "pam_succeed_if.so":
                self.assertNotIn(rule[2], ("requisite", "sufficient"), rule)

    def test_auth_order_mirrors_pambase_with_private_faillock(self):
        auth = stack(self.rules, "auth")
        self.assertEqual(
            [(rule[1], rule[2], rule[3]) for rule in auth],
            [
                (False, "required", "pam_succeed_if.so"),
                (False, "required", "pam_shells.so"),
                (False, "requisite", "pam_nologin.so"),
                (False, "required", "pam_faillock.so"),
                (True, "[success=2 default=ignore]", "pam_systemd_home.so"),
                (False, "[success=1 default=bad]", "pam_unix.so"),
                (False, "[default=die]", "pam_faillock.so"),
                (False, "optional", "pam_permit.so"),
                (False, "required", "pam_env.so"),
                (False, "required", "pam_faillock.so"),
            ],
        )
        modes = [rule[4][0] for rule in auth if rule[3] == "pam_faillock.so"]
        self.assertEqual(modes, ["preauth", "authfail", "authsucc"])
        # success=2 must skip exactly pam_unix and authfail.
        home = next(i for i, rule in enumerate(auth) if rule[3] == "pam_systemd_home.so")
        self.assertEqual([rule[3] for rule in auth[home + 1:home + 4]],
                         ["pam_unix.so", "pam_faillock.so", "pam_permit.so"])

    def test_every_faillock_line_uses_private_directory_and_limits(self):
        faillock = [rule for rule in self.rules if rule[3] == "pam_faillock.so"]
        self.assertEqual(len(faillock), 3)
        for rule in faillock:
            self.assertEqual(rule[0], "auth")
            self.assertEqual(rule[4][1:], FAILLOCK_ARGS, rule)
            self.assertNotIn("even_deny_root", rule[4])

    def test_pam_unix_keeps_failure_delay(self):
        # The uniform failure delay is what closes the timing oracle.
        for rule in self.rules:
            if rule[3] == "pam_unix.so":
                self.assertNotIn("nodelay", rule[4], rule)

    def test_account_and_session_mirror_previous_policy(self):
        self.assertEqual(
            [(rule[1], rule[2], rule[3]) for rule in stack(self.rules, "account")],
            [
                (False, "required", "pam_access.so"),
                (False, "required", "pam_nologin.so"),
                (True, "[success=1 default=ignore]", "pam_systemd_home.so"),
                (False, "required", "pam_unix.so"),
                (False, "optional", "pam_permit.so"),
                (False, "required", "pam_time.so"),
            ],
        )
        session = stack(self.rules, "session")
        self.assertEqual(
            [(rule[1], rule[2], rule[3], rule[4]) for rule in session],
            [
                (False, "required", "pam_loginuid.so", []),
                (False, "optional", "pam_keyinit.so", ["force", "revoke"]),
                (True, "optional", "pam_systemd_home.so", []),
                (False, "required", "pam_limits.so", []),
                (False, "required", "pam_unix.so", []),
                (False, "optional", "pam_permit.so", []),
                (False, "optional", "pam_umask.so", []),
                (False, "required", "pam_systemd.so", ["type=wayland", "class=user", "desktop=niri"]),
            ],
        )

    def test_policy_is_installed_as_backup_file(self):
        self.assertIn("config/niri-rdp.pam", (ROOT / "broker/Makefile").read_text())
        self.assertIn("'etc/pam.d/niri-rdp'", (ROOT / "packaging/arch/niri-rdp/PKGBUILD").read_text())

    def test_parser_handles_optional_and_bracketed_controls(self):
        rules = parse_pam("-auth [success=2 default=ignore] pam_x.so a b # c\n# x\n")
        self.assertEqual(rules, [("auth", True, "[success=2 default=ignore]", "pam_x.so", ["a", "b"])])


HARNESS = r"""
#define _POSIX_C_SOURCE 200809L
#include <security/pam_appl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static int conv(int n, const struct pam_message **m, struct pam_response **r, void *d) {
    struct pam_response *out = calloc((size_t)n, sizeof(*out));
    if (!out) return PAM_BUF_ERR;
    for (int i = 0; i < n; i++)
        if (m[i]->msg_style == PAM_PROMPT_ECHO_OFF || m[i]->msg_style == PAM_PROMPT_ECHO_ON)
            out[i].resp = strdup((const char *)d);
    *r = out;
    return PAM_SUCCESS;
}
int main(int argc, char **argv) {
    if (argc != 5) return 2;
    struct pam_conv c = {.conv = conv, .appdata_ptr = argv[4]};
    pam_handle_t *h = NULL;
    const char *dir = strcmp(argv[1], "-") ? argv[1] : NULL;
    if (pam_start_confdir(argv[2], argv[3], &c, dir, &h) != PAM_SUCCESS) return 3;
    pam_set_item(h, PAM_RHOST, "rdp-loopback");
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    int rc = pam_authenticate(h, PAM_DISALLOW_NULL_AUTHTOK);
    clock_gettime(CLOCK_MONOTONIC, &b);
    int acct = rc == PAM_SUCCESS ? pam_acct_mgmt(h, 0) : -1;
    long ms = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
    printf("%d %ld %d\n", rc, ms, acct);
    pam_end(h, rc);
    return 0;
}
"""

ACCOUNTS = {
    # name: (uid, shell, password)
    "root": (0, "/usr/bin/bash", "root-secret"),
    "niri-rdp": (960, "/usr/bin/nologin", "service-secret"),
    "system": (961, "/usr/bin/bash", "system-secret"),
    "alice": (1000, "/usr/bin/bash", "alice-secret"),
    "bob": (1001, "/usr/bin/bash", "bob-secret"),
}
PAM_SUCCESS, PAM_AUTH_ERR = 0, 7
MODULES = Path("/usr/lib/security")


class PamLabTests(unittest.TestCase):
    """Run the real stack against fake accounts in a private user namespace.

    The policy is loaded from a private confdir, /etc/passwd, /etc/group and
    /etc/shadow are disposable copies and /run is a temporary directory, so
    host PAM state and tallies are never read or written.
    """

    @classmethod
    def setUpClass(cls):
        tools = [shutil.which(name) for name in ("bwrap", "cc", "openssl")]
        if not all(tools) or not (MODULES / "pam_faillock.so").exists():
            raise unittest.SkipTest("needs bwrap, cc, openssl and Arch PAM modules")
        cls.tmp = tempfile.TemporaryDirectory(prefix="niri-rdp-pam-")
        base = Path(cls.tmp.name)
        harness = base / "harness"
        build = subprocess.run(["cc", "-std=c17", "-Wall", "-Wextra", "-Werror", "-x", "c",
                                "-o", str(harness), "-", "-lpam"],
                               input=HARNESS, text=True, capture_output=True)
        if build.returncode:
            cls.tmp.cleanup()
            raise unittest.SkipTest(f"cannot build PAM harness: {build.stderr.strip()}")
        etc = base / "etc"
        etc.mkdir()
        (base / "conf").mkdir()
        shutil.copy(PAM_FILE, base / "conf/niri-rdp")
        passwd, shadow, group = [], [], []
        for name, (uid, shell, password) in ACCOUNTS.items():
            digest = subprocess.run(["openssl", "passwd", "-6", "-stdin"], input=password,
                                    text=True, capture_output=True, check=True).stdout.strip()
            passwd.append(f"{name}:x:{uid}:{uid}::/:{shell}")
            shadow.append(f"{name}:{digest}:20000::::::")
            group.append(f"{name}:x:{uid}:")
        # Every account is a member so the UID test is exercised separately.
        group.append("niri-rdp-users:x:970:root,niri-rdp,system,alice")
        (etc / "passwd").write_text("\n".join(passwd) + "\n")
        (etc / "shadow").write_text("\n".join(shadow) + "\n")
        (etc / "group").write_text("\n".join(group) + "\n")
        os.chmod(etc / "shadow", 0o600)
        cls.base = base
        probe = subprocess.run(cls.command(base / "probe-run", "/usr/bin/true"), capture_output=True)
        if probe.returncode:
            cls.tmp.cleanup()
            raise unittest.SkipTest("unprivileged user namespaces are unavailable")

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    @classmethod
    def command(cls, run, *argv):
        run.mkdir(exist_ok=True)
        base = cls.base
        args = ["bwrap", "--unshare-all", "--unshare-user", "--uid", "0", "--gid", "0",
                "--die-with-parent", "--new-session", "--ro-bind", "/usr", "/usr",
                "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib", "/lib64",
                "--symlink", "usr/bin", "/bin", "--proc", "/proc", "--dev", "/dev",
                "--tmpfs", "/tmp", "--dir", "/etc"]
        for name in ("passwd", "group", "shadow"):
            args += ["--ro-bind", str(base / "etc" / name), f"/etc/{name}"]
        for path in ("/etc/pam.d", "/etc/security", "/etc/shells", "/etc/nsswitch.conf",
                     "/etc/login.defs"):
            if Path(path).exists():
                args += ["--ro-bind", path, path]
        args += ["--bind", str(run), "/run", "--dir", "/var", "--symlink", "../run", "/var/run",
                 "--ro-bind", str(base / "conf"), "/conf",
                 "--ro-bind", str(base / "harness"), "/harness",
                 "--clearenv", "--setenv", "PATH", "/usr/bin"]
        return args + list(argv)

    def start(self, run, user, password, service="niri-rdp", confdir="/conf"):
        return subprocess.Popen(self.command(run, "/harness", confdir, service, user, password),
                                stdout=subprocess.PIPE, text=True)

    @staticmethod
    def result(process):
        output, _ = process.communicate(timeout=30)
        assert process.returncode == 0, output
        rc, ms, acct = (int(value) for value in output.split())
        return rc, ms, acct

    def attempt(self, run, user, password, **kwargs):
        return self.result(self.start(run, user, password, **kwargs))

    def test_rejected_accounts_fail_like_wrong_passwords(self):
        run = self.base / "run-gate"
        cases = [(user, ACCOUNTS[user][2]) for user in ("root", "niri-rdp", "system", "bob")]
        cases += [("root", "wrong"), ("bob", "wrong"), ("alice", "wrong")]
        # Distinct users keep separate tallies, so the attempts run together.
        processes = [(case, self.start(run, *case)) for case in cases]
        for case, process in processes:
            rc, ms, _ = self.result(process)
            with self.subTest(case=case[0], correct=case[1] != "wrong"):
                self.assertEqual(rc, PAM_AUTH_ERR)
                # pam_unix requests 2 s; libpam randomises it by about 25%.
                self.assertGreaterEqual(ms, 1000)
        rc, ms, acct = self.attempt(run, "alice", ACCOUNTS["alice"][2])
        self.assertEqual((rc, acct), (PAM_SUCCESS, PAM_SUCCESS))
        self.assertLess(ms, 1000)

    def test_private_lockout_after_five_failures_does_not_lock_sudo(self):
        run = self.base / "run-lockout"
        password = ACCOUNTS["alice"][2]
        for _ in range(4):
            self.assertEqual(self.attempt(run, "alice", "wrong")[0], PAM_AUTH_ERR)
        # Four failures stay below deny=5, and success resets the tally.
        self.assertEqual(self.attempt(run, "alice", password)[0], PAM_SUCCESS)
        for _ in range(5):
            self.assertEqual(self.attempt(run, "alice", "wrong")[0], PAM_AUTH_ERR)
        self.assertEqual(self.attempt(run, "alice", password)[0], PAM_AUTH_ERR)
        self.assertTrue((run / "faillock-niri-rdp/alice").is_file())
        self.assertFalse((run / "faillock/alice").exists())
        if Path("/etc/pam.d/sudo").exists():
            # The host's own sudo stack (read-only) uses the default tally.
            rc = self.attempt(run, "alice", password, service="sudo", confdir="-")[0]
            self.assertEqual(rc, PAM_SUCCESS)


if __name__ == "__main__":
    unittest.main()
