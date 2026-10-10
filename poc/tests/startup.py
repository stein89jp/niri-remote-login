#!/usr/bin/env python3
"""Fail-closed startup tests. These checks never open a listening socket."""
import os
from pathlib import Path
import subprocess
import unittest

BINARY = Path(__file__).resolve().parents[1] / "target/debug/niri-rdp-transport-poc"


class Startup(unittest.TestCase):
    def run_failure(self, variables, expected, arguments=()):
        self.assertTrue(BINARY.is_file(), "run cargo build --locked first")
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith("POC_") and key != "SSLKEYLOGFILE"}
        environment.update(variables)
        result = subprocess.run([str(BINARY), *arguments], env=environment,
                                capture_output=True, text=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(expected, result.stderr)
        self.assertNotIn("Synthetic RDP test card on", result.stdout)

    def test_requires_explicit_tls_identity(self):
        self.run_failure({}, "set POC_CERT explicitly")
        self.run_failure({"POC_CERT": "/missing/cert.pem"}, "set POC_KEY explicitly")

    def test_requires_nonempty_throwaway_credentials(self):
        identity = {"POC_CERT": "/missing/cert.pem", "POC_KEY": "/missing/key.pem"}
        self.run_failure(identity, "set POC_USERNAME explicitly")
        self.run_failure(dict(identity, POC_USERNAME="throwaway"), "set POC_PASSWORD explicitly")
        self.run_failure(dict(identity, POC_USERNAME="throwaway", POC_PASSWORD=""),
                         "POC_PASSWORD must not be empty")

    def test_no_unencrypted_fallback_for_invalid_identity(self):
        self.run_failure({"POC_CERT": "/missing/cert.pem", "POC_KEY": "/missing/key.pem",
                          "POC_USERNAME": "throwaway", "POC_PASSWORD": "unused-test-value"},
                         "reading server cert")

    def test_refuses_cli_options_and_tls_key_logging(self):
        self.run_failure({}, "no arguments accepted", ("--bind-address=0.0.0.0:3389",))
        self.run_failure({"SSLKEYLOGFILE": "/tmp/never-created-by-this-test"},
                         "unset SSLKEYLOGFILE")


if __name__ == "__main__":
    unittest.main()
