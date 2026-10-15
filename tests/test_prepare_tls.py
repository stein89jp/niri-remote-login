"""Exercise TLS certificate renewal and expiry warnings without changing the host."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
OPENSSL = Path("/usr/bin/openssl")


def load_prepare_tls():
    spec = importlib.util.spec_from_file_location("prepare_tls_renewal", ROOT / "packaging" / "prepare-tls.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


tls = load_prepare_tls()


def openssl(*arguments):
    return subprocess.run([str(OPENSSL), *map(str, arguments)], check=True, capture_output=True).stdout


def supports_not_after():
    if not OPENSSL.exists():
        return False
    result = subprocess.run([str(OPENSSL), "req", "-help"], capture_output=True, text=True)
    return "-not_after" in result.stdout + result.stderr


def write_identity(directory, hostname="arch.local", validity=("-days", "1"), san=None):
    """Write a self-signed identity with the given validity arguments, as a user would."""
    san = san or tls.identity_names(hostname)[1]
    cert, key = directory / "cert.pem", directory / "key.pem"
    openssl("req", "-x509", "-newkey", "rsa:3072", "-nodes", *validity,
            "-subj", f"/CN={hostname}", "-addext", f"subjectAltName={san}",
            "-keyout", key, "-out", cert)
    key.chmod(0o600)
    cert.chmod(0o644)
    return cert, key


def mark_generated(directory, hostname="arch.local"):
    fingerprint = tls.certificate_fingerprint(directory / "cert.pem")
    marker = directory / tls.MARKER_NAME
    marker.write_text(json.dumps({"sha256_fingerprint": fingerprint, "hostname": hostname}) + "\n")
    marker.chmod(0o644)
    return fingerprint


class Log:
    def __init__(self):
        self.messages = []

    def __call__(self, message, warning=False):
        self.messages.append((message, warning))

    def warnings(self):
        return [message for message, warning in self.messages if warning]


@unittest.skipUnless(OPENSSL.exists(), "OpenSSL not installed")
class PrepareTlsRenewalTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name) / "tls"
        self.directory.mkdir(mode=0o700)
        self.uid = os.getuid()

    def tearDown(self):
        self.temporary.cleanup()

    def ensure(self, log=None, **options):
        return tls.ensure_tls(self.directory, "arch.local", self.uid, log=log or Log(), **options)

    def assert_consistent_generated_identity(self):
        cert, key = self.directory / "cert.pem", self.directory / "key.pem"
        self.assertEqual(openssl("x509", "-in", cert, "-pubkey", "-noout"),
                         openssl("pkey", "-in", key, "-pubout"))
        record = json.loads((self.directory / tls.MARKER_NAME).read_text())
        self.assertEqual(record["sha256_fingerprint"], tls.certificate_fingerprint(cert))
        self.assertEqual(key.stat().st_mode & 0o777, 0o600)
        self.assertEqual(cert.stat().st_mode & 0o777, 0o644)
        self.assertEqual((self.directory / tls.MARKER_NAME).stat().st_mode & 0o777, 0o644)
        self.assertFalse((self.directory / tls.PENDING_NAME).exists())
        self.assertEqual(list(self.directory.glob(".new-*")), [])
        return record

    def test_new_identity_is_marked_long_lived_and_logs_fingerprint(self):
        log = Log()
        self.assertTrue(self.ensure(log))
        record = self.assert_consistent_generated_identity()
        self.assertEqual(record["hostname"], "arch.local")
        self.assertIn(record["sha256_fingerprint"], log.messages[0][0])
        self.assertEqual(log.warnings(), [])
        cert = self.directory / "cert.pem"
        self.assertFalse(tls.expires_within(cert, (tls.CERT_DAYS - 1) * 86400))
        self.assertTrue(tls.expires_within(cert, (tls.CERT_DAYS + 1) * 86400))
        self.assertIn("Public-Key: (3072 bit)", openssl("x509", "-in", cert, "-noout", "-text").decode())

    def test_generated_identity_renews_inside_threshold_and_keeps_recorded_name(self):
        write_identity(self.directory, "renew.example")
        old_fingerprint = mark_generated(self.directory, "renew.example")
        log = Log()
        self.assertTrue(self.ensure(log))
        record = self.assert_consistent_generated_identity()
        self.assertNotEqual(record["sha256_fingerprint"], old_fingerprint)
        self.assertEqual(record["hostname"], "renew.example")
        self.assertIn(record["sha256_fingerprint"], log.messages[0][0])
        self.assertIn("Renewed", log.messages[0][0])
        result = openssl("x509", "-in", self.directory / "cert.pem", "-noout", "-checkhost", "renew.example")
        self.assertIn(b"does match", result)
        # The renewed identity is stable on the next start.
        self.assertFalse(self.ensure())
        self.assertEqual(json.loads((self.directory / tls.MARKER_NAME).read_text()), record)

    def test_generated_identity_outside_threshold_is_preserved(self):
        cert, key = write_identity(self.directory, validity=("-days", "90"))
        mark_generated(self.directory)
        before = cert.read_bytes(), key.read_bytes()
        self.assertFalse(self.ensure())
        self.assertEqual((cert.read_bytes(), key.read_bytes()), before)

    @unittest.skipUnless(supports_not_after(), "OpenSSL lacks -not_after")
    def test_expired_generated_identity_is_renewed(self):
        write_identity(self.directory, validity=("-not_before", "20200101000000Z",
                                                 "-not_after", "20200102000000Z"))
        mark_generated(self.directory)
        self.assertTrue(self.ensure())
        self.assert_consistent_generated_identity()
        openssl("x509", "-in", self.directory / "cert.pem", "-noout", "-checkend", "0")

    def test_user_identity_near_expiry_warns_and_is_preserved(self):
        cert, key = write_identity(self.directory)
        before = cert.read_bytes(), key.read_bytes()
        log = Log()
        self.assertFalse(self.ensure(log))
        self.assertEqual((cert.read_bytes(), key.read_bytes()), before)
        self.assertEqual(len(log.warnings()), 1)
        self.assertIn("will not be renewed", log.warnings()[0])
        self.assertFalse((self.directory / tls.MARKER_NAME).exists())

    def test_user_identity_outside_threshold_does_not_warn(self):
        write_identity(self.directory, validity=("-days", "90"))
        log = Log()
        self.assertFalse(self.ensure(log))
        self.assertEqual(log.warnings(), [])

    @unittest.skipUnless(supports_not_after(), "OpenSSL lacks -not_after")
    def test_expired_user_identity_still_fails_and_is_preserved(self):
        cert, key = write_identity(self.directory, validity=("-not_before", "20200101000000Z",
                                                             "-not_after", "20200102000000Z"))
        before = cert.read_bytes(), key.read_bytes()
        with self.assertRaisesRegex(ValueError, "expired"):
            self.ensure()
        self.assertEqual((cert.read_bytes(), key.read_bytes()), before)

    def test_replaced_certificate_with_stale_marker_is_treated_as_user_provided(self):
        write_identity(self.directory, validity=("-days", "90"))
        mark_generated(self.directory)
        cert, key = write_identity(self.directory)
        before = cert.read_bytes(), key.read_bytes()
        log = Log()
        self.assertFalse(self.ensure(log))
        self.assertEqual((cert.read_bytes(), key.read_bytes()), before)
        self.assertEqual(len(log.warnings()), 1)

    def test_unsafe_marker_is_rejected(self):
        write_identity(self.directory)
        mark_generated(self.directory)
        (self.directory / tls.MARKER_NAME).chmod(0o666)
        with self.assertRaisesRegex(ValueError, "unsafe permissions"):
            self.ensure()

    def test_unmarked_identity_from_earlier_releases_is_adopted(self):
        cert, _ = write_identity(self.directory, validity=("-days", str(tls.LEGACY_CERT_DAYS)))
        old_fingerprint = tls.certificate_fingerprint(cert)
        # Not yet near expiry: kept as-is, without a warning.
        log = Log()
        self.assertFalse(self.ensure(log))
        self.assertEqual(log.warnings(), [])
        self.assertEqual(tls.certificate_fingerprint(cert), old_fingerprint)
        # Inside the threshold: renewed and marked.
        self.assertTrue(self.ensure(renew_before=(tls.LEGACY_CERT_DAYS + 1) * 86400))
        record = self.assert_consistent_generated_identity()
        self.assertNotEqual(record["sha256_fingerprint"], old_fingerprint)
        self.assertEqual(record["hostname"], "arch.local")

    def test_user_identity_resembling_earlier_releases_is_not_adopted(self):
        legacy_days = ("-days", str(tls.LEGACY_CERT_DAYS))
        for validity, san in ((("-days", "364"), None), (legacy_days, "DNS:arch.local")):
            with self.subTest(validity=validity, san=san):
                cert, _ = write_identity(self.directory, "arch.local", validity, san)
                before = cert.read_bytes()
                log = Log()
                self.assertFalse(self.ensure(log, renew_before=400 * 86400))
                self.assertEqual(cert.read_bytes(), before)
                self.assertEqual(len(log.warnings()), 1)

    def test_failed_generation_preserves_identity_and_cleans_staging(self):
        cert, key = write_identity(self.directory)
        mark_generated(self.directory)
        before = cert.read_bytes(), key.read_bytes(), (self.directory / tls.MARKER_NAME).read_bytes()
        real_openssl = tls.run_openssl

        def failing_openssl(*arguments):
            if arguments[0] == "req":
                raise subprocess.CalledProcessError(1, "openssl req")
            return real_openssl(*arguments)

        with patch.object(tls, "run_openssl", failing_openssl), \
                self.assertRaises(subprocess.CalledProcessError):
            self.ensure()
        self.assertEqual((cert.read_bytes(), key.read_bytes(),
                          (self.directory / tls.MARKER_NAME).read_bytes()), before)
        self.assertFalse((self.directory / tls.PENDING_NAME).exists())
        self.assertEqual(list(self.directory.glob(".new-*")), [])

    def test_interrupted_commit_is_completed_on_next_start(self):
        write_identity(self.directory)
        old_fingerprint = mark_generated(self.directory)
        real_replace = Path.replace
        calls = []

        def crash_after_key(source, target):
            calls.append(Path(target).name)
            if len(calls) == 2:
                raise OSError("simulated power loss")
            return real_replace(source, target)

        with patch.object(Path, "replace", crash_after_key), \
                self.assertRaisesRegex(OSError, "simulated power loss"):
            self.ensure()
        self.assertEqual(calls, ["key.pem", "cert.pem"])
        # The live pair is mismatched, but the complete staged identity remains.
        self.assertTrue((self.directory / tls.PENDING_NAME / "cert.pem").exists())
        self.assertFalse(self.ensure())
        record = self.assert_consistent_generated_identity()
        self.assertNotEqual(record["sha256_fingerprint"], old_fingerprint)

    def test_abandoned_staging_directory_is_removed(self):
        self.ensure()
        abandoned = self.directory / ".new-abandoned"
        abandoned.mkdir()
        (abandoned / "key.pem").write_text("partial")
        self.assertFalse(self.ensure())
        self.assertFalse(abandoned.exists())

    def test_pending_symlink_is_rejected(self):
        self.ensure()
        (Path(self.temporary.name) / "elsewhere").mkdir()
        (self.directory / tls.PENDING_NAME).symlink_to(Path(self.temporary.name) / "elsewhere")
        with self.assertRaisesRegex(ValueError, "root-owned directory"):
            self.ensure()

    def test_warning_uses_journal_priority_only_under_systemd(self):
        with patch.dict(os.environ, {"JOURNAL_STREAM": "8:1234"}), \
                patch.object(tls.sys, "stderr") as stderr:
            tls.report("expiring", warning=True)
        self.assertTrue(stderr.write.call_args_list[0].args[0].startswith("<4>warning: expiring"))
        environment = {key: value for key, value in os.environ.items() if key != "JOURNAL_STREAM"}
        with patch.dict(os.environ, environment, clear=True), patch.object(tls.sys, "stderr") as stderr:
            tls.report("expiring", warning=True)
        self.assertTrue(stderr.write.call_args_list[0].args[0].startswith("warning: expiring"))


if __name__ == "__main__":
    unittest.main()
