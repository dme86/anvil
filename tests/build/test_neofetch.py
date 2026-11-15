import os
from pathlib import Path
import runpy
import stat
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/configure-neofetch.py"
HOOK = ROOT / "contrib/neofetch/anvil.conf"


class NeofetchIntegrationTests(unittest.TestCase):
    def invoke(self, *args, env=None, check=True):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *map(str, args)],
            env=env, check=check, capture_output=True, text=True,
        )

    def test_preserves_user_settings_permissions_and_first_backup(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory, "config.conf")
            original = b'print_info() { info "WM" wm; }\n# my settings\n'
            config.write_bytes(original)
            config.chmod(0o640)
            self.invoke("--config", config)
            first = config.read_bytes()
            self.assertTrue(first.startswith(original))
            self.assertEqual(first.count(b"# BEGIN Anvil session identity"), 1)
            backup = config.with_name(config.name + ".before-anvil-wm")
            self.assertEqual(backup.read_bytes(), original)
            self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o640)
            self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o640)

            self.invoke("--config", config)
            self.assertEqual(config.read_bytes(), first)
            config.write_bytes(first + b"# later user customization\n")
            self.invoke("--config", config)
            self.assertTrue(config.read_bytes().endswith(b"# later user customization\n"))
            self.assertEqual(backup.read_bytes(), original)

    def test_missing_config_uses_xdg_path_and_explicit_path_wins(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = dict(os.environ, HOME=str(root / "home"), XDG_CONFIG_HOME=str(root / "xdg"))
            self.invoke(env=env)
            config = root / "xdg/neofetch/config.conf"
            self.assertIn("wm_run=1", config.read_text())
            self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o600)
            self.assertFalse(config.with_name(config.name + ".before-anvil-wm").exists())
            explicit = root / "custom config" / "neofetch.conf"
            self.invoke("--config", explicit, env=env)
            self.assertTrue(explicit.is_file())
            self.assertFalse((root / "home/.config/neofetch/config.conf").exists())

    def test_home_fallback_and_relative_xdg_rejection(self):
        with tempfile.TemporaryDirectory() as directory:
            env = dict(os.environ, HOME=directory)
            env.pop("XDG_CONFIG_HOME", None)
            self.invoke(env=env)
            self.assertTrue(Path(directory, ".config/neofetch/config.conf").is_file())
            env["XDG_CONFIG_HOME"] = "relative-config"
            result = self.invoke(env=env, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("absolute path", result.stderr)

    def test_dotfile_symlink_and_crlf_settings_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "dotfiles/neofetch.conf"
            target.parent.mkdir()
            original = b'# custom Windows-style line endings\r\nseparator=" :: "\r\n'
            target.write_bytes(original)
            config = root / "config.conf"
            config.symlink_to(target)
            self.invoke("--config", config)
            self.assertTrue(config.is_symlink())
            result = target.read_bytes()
            self.assertTrue(result.startswith(original))
            self.assertNotIn(b"\n", result.replace(b"\r\n", b""))
            self.assertEqual(target.with_name(target.name + ".before-anvil-wm").read_bytes(), original)

    def test_existing_marked_block_is_updated_and_unfinished_block_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory, "config.conf")
            config.write_text("# user prefix\n# BEGIN Anvil session identity\nold=true\n# END Anvil session identity\n# user suffix\n")
            self.invoke("--config", config)
            result = config.read_text()
            self.assertNotIn("old=true", result)
            self.assertTrue(result.startswith("# user prefix\n"))
            self.assertTrue(result.endswith("# user suffix\n"))
            broken = "# user prefix\n# BEGIN Anvil session identity\ncustom=true\n"
            config.write_text(broken)
            self.assertNotEqual(self.invoke("--config", config, check=False).returncode, 0)
            self.assertEqual(config.read_text(), broken)

    def test_hook_uses_exact_session_identity_and_leaves_other_desktops_unchanged(self):
        embedded = runpy.run_path(str(SCRIPT))["HOOK"]
        self.assertEqual(HOOK.read_text().rstrip("\n"), embedded)
        subprocess.run(["bash", "-n", str(HOOK)], check=True)
        for desktop, session_type, expected in [
            ("anvil", "wayland", "Anvil:1"),
            ("Anvil", "wayland", "Anvil:1"),
            ("GNOME:ANVIL", "wayland", "Anvil:1"),
            ("sway", "wayland", "sway:0"),
            ("notanvil", "wayland", "sway:0"),
            ("anvil", "x11", "sway:0"),
        ]:
            env = dict(os.environ, XDG_CURRENT_DESKTOP=desktop, XDG_SESSION_TYPE=session_type)
            result = subprocess.run(
                ["bash", "-c", 'wm=sway; wm_run=0; source "$1"; printf "%s:%s" "$wm" "$wm_run"', "bash", str(HOOK)],
                env=env, check=True, capture_output=True, text=True,
            )
            self.assertEqual(result.stdout, expected, (desktop, session_type))


if __name__ == "__main__":
    unittest.main()
