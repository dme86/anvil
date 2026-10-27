"""Smoke checks for examples installed with the asynchronous bar (issue #72)."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = sorted((ROOT / "scripts/bar").glob("*.sh"))


class BarExamplesTests(unittest.TestCase):
    def test_all_bash_examples_parse(self):
        self.assertTrue(SCRIPTS)
        for script in SCRIPTS:
            with self.subTest(script=script.name):
                subprocess.run(["bash", "-n", str(script)], check=True)

    def test_gentoo_updates_does_not_sync_or_require_root(self):
        with tempfile.TemporaryDirectory() as root:
            bin_dir = Path(root)
            mock = bin_dir / "emerge"
            args_path = bin_dir / "arguments"
            mock.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$@" > "$MOCK_EMERGE_ARGS"\n'
                "printf '[ebuild  U  ] app-editors/vim\\n'\n"
                "printf '[ebuild  N  ] dev-libs/foo\\n'\n"
            )
            mock.chmod(0o755)
            env = os.environ.copy()
            env["MOCK_EMERGE_ARGS"] = str(args_path)
            env["PATH"] = f"{bin_dir}:{env['PATH']}"
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/bar/gentoo_updates.sh")],
                env=env,
                text=True,
                capture_output=True,
                check=True,
                timeout=5,
            )
            self.assertTrue(result.stdout.strip().endswith("2"))
            args = args_path.read_text()
            self.assertIn("--pretend", args)
            self.assertNotIn("--sync", args)
            self.assertNotIn("sudo", args)

    def test_failed_emerge_does_not_emit_fake_zero(self):
        with tempfile.TemporaryDirectory() as root:
            mock = Path(root) / "emerge"
            mock.write_text("#!/bin/sh\nexit 1\n")
            mock.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = f"{root}:{env['PATH']}"
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/bar/gentoo_updates.sh")],
                env=env,
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
