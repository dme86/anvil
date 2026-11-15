#!/usr/bin/env python3
"""Install Anvil's session-aware Neofetch hook without replacing user settings."""

import argparse
import os
from pathlib import Path
import stat
import tempfile


BEGIN = "# BEGIN Anvil session identity"
END = "# END Anvil session identity"
HOOK = """# BEGIN Anvil session identity
# Neofetch's process substring detection mistakes swaybg for the Sway compositor.
# Trust the explicit session identity only when this is an Anvil Wayland session.
if [[ ${XDG_SESSION_TYPE:-} == wayland ]]; then
    case ":${XDG_CURRENT_DESKTOP,,}:" in
        *:anvil:*) wm="Anvil"; wm_run=1 ;;
    esac
fi
# END Anvil session identity"""


def config_path():
    root = os.environ.get("XDG_CONFIG_HOME")
    if root:
        path = Path(root).expanduser()
        if not path.is_absolute():
            raise ValueError("XDG_CONFIG_HOME must be an absolute path")
    else:
        path = Path.home() / ".config"
    return path / "neofetch" / "config.conf"


def add_hook(text):
    """Replace our own marked blocks, retaining all other content byte-for-byte."""
    newline = "\r\n" if "\r\n" in text else "\n"
    block = HOOK.replace("\n", newline) + newline
    result = []
    inside = False
    found = False
    for line in text.splitlines(keepends=True):
        marker = line.rstrip("\r\n")
        if marker == BEGIN:
            if inside:
                raise ValueError("nested Anvil markers; configuration was not changed")
            inside = True
            if not found:
                result.append(block)
                found = True
        elif marker == END:
            if not inside:
                raise ValueError("unmatched Anvil marker; configuration was not changed")
            inside = False
        elif not inside:
            result.append(line)
    if inside:
        raise ValueError("unfinished Anvil block; configuration was not changed")
    if found:
        return "".join(result)
    separator = (newline if text.endswith("\n") else newline * 2) if text else ""
    return text + separator + block


def set_metadata(fd, previous):
    if previous is None:
        os.fchmod(fd, 0o600)
        return
    current = os.fstat(fd)
    if (current.st_uid, current.st_gid) != (previous.st_uid, previous.st_gid):
        os.fchown(fd, previous.st_uid, previous.st_gid)
    os.fchmod(fd, stat.S_IMODE(previous.st_mode))


def configure(requested):
    # Dotfile symlinks keep pointing to their original target.
    target = requested.expanduser().resolve()
    if target.exists() and not target.is_file():
        raise ValueError(f"configuration is not a regular file: {target}")
    previous = target.stat() if target.exists() else None
    original = target.read_bytes() if previous is not None else b""
    updated = add_hook(original.decode("utf-8")).encode("utf-8")
    if updated == original:
        return target, None, False

    target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    backup = None
    if previous is not None:
        backup = target.with_name(target.name + ".before-anvil-wm")
        try:
            # Never replace an existing backup, including a symlink.
            with backup.open("xb") as stream:
                set_metadata(stream.fileno(), previous)
                stream.write(original)
                stream.flush()
                os.fsync(stream.fileno())
        except FileExistsError:
            backup = None

    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb", dir=target.parent, prefix=f".{target.name}.anvil-", delete=False
        ) as stream:
            temporary = Path(stream.name)
            set_metadata(stream.fileno(), previous)
            stream.write(updated)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, target)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return target, backup, True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config", type=Path,
        help="Neofetch config to update (default: $XDG_CONFIG_HOME/neofetch/config.conf or ~/.config/neofetch/config.conf)",
    )
    args = parser.parse_args()
    try:
        path, backup, changed = configure(args.config if args.config is not None else config_path())
    except (OSError, UnicodeError, ValueError, RuntimeError) as error:
        parser.exit(1, f"Cannot configure Neofetch: {error}\n")
    print(f"{'Configured' if changed else 'Already configured'} Neofetch for Anvil: {path}")
    if backup is not None:
        print(f"Original configuration saved to: {backup}")


if __name__ == "__main__":
    main()
