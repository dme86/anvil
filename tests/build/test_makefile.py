"""Exercise Makefile UX/install variants without compiling Smithay or using root."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO = Path(os.environ.get("ANVIL_MAKE_ROOT", Path(__file__).resolve().parents[2]))
FAKE_CARGO = """#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "$CARGO_LOG"
case "$1" in
  build)
    mkdir -p "$CARGO_TARGET_DIR/release"
    printf '#!/bin/sh\\n# %s\\n' "$*" > "$CARGO_TARGET_DIR/release/anvil"
    chmod 755 "$CARGO_TARGET_DIR/release/anvil"
    case " $* " in
      *--no-default-features*)
        case " $* " in
          *anvilctl*) printf '#!/bin/sh\\n# CLI\\n' > "$CARGO_TARGET_DIR/release/anvilctl";
                    chmod 755 "$CARGO_TARGET_DIR/release/anvilctl" ;;
        esac ;;
      *) printf '#!/bin/sh\\n# CLI\\n' > "$CARGO_TARGET_DIR/release/anvilctl";
         chmod 755 "$CARGO_TARGET_DIR/release/anvilctl" ;;
    esac
    ;;
  clean) rm -rf "$CARGO_TARGET_DIR" ;;
esac
"""


class MakefileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.cargo = self.base / "fake-cargo"
        self.cargo.write_text(FAKE_CARGO)
        self.cargo.chmod(0o755)
        self.target = self.base / "target"
        self.log = self.base / "cargo.log"
        self.stage = self.base / "stage"
        self.env = dict(os.environ, CARGO_LOG=str(self.log))
        self.args = [
            "make", "--no-print-directory", "-C", str(REPO),
            f"CARGO={self.cargo}", f"CARGO_TARGET_DIR={self.target}",
            f"DESTDIR={self.stage}", "PREFIX=/opt/anvil",
            "SESSION_DIR=/share/wayland-sessions",
        ]

    def make(self, target=None, success=True):
        args = self.args + ([target] if target else [])
        result = subprocess.run(args, env=self.env, text=True, capture_output=True, check=False)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout + result.stderr

    def test_default_and_help_document_targets_without_building(self):
        for target in (None, "help"):
            help_text = self.make(target)
            for label in ("build", "build-no-bar", "build-minimal", "install", "uninstall", "check"):
                self.assertIn(label, help_text)
        self.assertFalse(self.log.exists(), "help unexpectedly ran Cargo")

    def test_profiles_install_correct_binaries_and_session(self):
        cases = {
            "build": ("default", "--release --locked", True),
            "build-no-bar": ("no-bar", "--no-default-features --features anvilctl,launcher,overview,layer-shell", True),
            "build-minimal": ("minimal", "--no-default-features", False),
            "build-xwayland": ("xwayland", "--features xwayland", True),
            "build-all": ("all", "--all-features", True),
        }
        for target, (profile, flags, has_cli) in cases.items():
            with self.subTest(target=target):
                self.make(target)
                self.assertEqual((self.target / ".anvil-make-variant").read_text().strip(), profile)
                self.assertIn(flags, self.log.read_text())
                before_install = self.log.read_text()
                self.make("install")
                self.assertEqual(self.log.read_text(), before_install, "install must not compile as root")
                bin_dir = self.stage / "opt/anvil/bin"
                self.assertTrue((bin_dir / "anvil").is_file())
                if has_cli:
                    self.assertTrue((bin_dir / "anvilctl").is_file())
                desktop = (self.stage / "share/wayland-sessions/anvil.desktop").read_text()
                self.assertIn("Exec=/opt/anvil/bin/anvil", desktop)
                self.assertNotIn(str(self.stage), desktop)
                self.make("uninstall")
                self.assertFalse((bin_dir / "anvil").exists())
                self.assertFalse((bin_dir / "anvilctl").exists())
                self.assertFalse((self.stage / "share/wayland-sessions/anvil.desktop").exists())

    def test_install_rejects_unverified_or_modified_binary(self):
        text = self.make("install", success=False)
        self.assertIn("make build", text)
        self.make("build-no-bar")
        (self.target / "release/anvil").write_text("different contents")
        text = self.make("install", success=False)
        self.assertIn("changed", text)
        self.assertFalse((self.stage / "opt/anvil/bin/anvil").exists())

    def test_minimal_does_not_install_old_cli(self):
        self.make("build")
        self.make("build-minimal")  # Fake Cargo deliberately leaves older anvilctl in target/release.
        self.make("install")
        self.assertTrue((self.stage / "opt/anvil/bin/anvil").exists())
        self.assertFalse((self.stage / "opt/anvil/bin/anvilctl").exists())


if __name__ == "__main__":
    unittest.main()
