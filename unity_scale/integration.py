"""Opt-in, reversible launch interception for a single installed Unity Editor.

Only the executable directory is touched: the original file is hard-linked to a
backup, and an atomic replacement installs a small, self-contained launcher.
The Editor's compiled bytes are never rewritten. No Unity Hub database is edited.
"""
from __future__ import annotations

from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import tempfile
from typing import Iterator

from .core import AppError, ROOT, Store, editor_key

_OWNER = "io.github.unityscale.UnityScale.integration"
_SCHEMA = 1
_BACKUP = "Unity.unity-scale-original"
_MANIFEST = ".Unity.unity-scale.json"
_MARKER = "UNITY_SCALE_MANAGED_WRAPPER"


def _paths(editor_path: str | Path) -> tuple[Path, Path, Path]:
    editor = Path(editor_path).expanduser()
    if editor.is_dir():
        editor = next((p for p in (editor / "Editor/Unity", editor / "Unity") if p.exists()), editor / "Editor/Unity")
    editor = editor.resolve()
    if editor.name != "Unity":
        raise AppError("Select the Unity Editor executable named Unity.")
    return editor, editor.with_name(_BACKUP), editor.with_name(_MANIFEST)


def _digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def _mode(path: Path) -> int:
    return stat.S_IMODE(path.stat().st_mode)


def _regular(path: Path) -> bool:
    return not path.is_symlink() and path.is_file()


def _exists(path: Path) -> bool:
    return os.path.lexists(path)


def _record_path(editor: Path) -> Path:
    return Store().state_dir / "integrations" / (editor_key(editor) + ".json")


def _write_json(path: Path, value: dict, *, exclusive: bool = False) -> None:
    data = (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode()
    if exclusive:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
        except BaseException:
            path.unlink(missing_ok=True)
            raise
        return
    path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".integration-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def _read_manifest(editor: Path, manifest: Path) -> dict:
    if not _regular(manifest) or manifest.stat().st_size > 16384:
        raise AppError("The scaling integration record is not a regular, valid file.")
    try:
        value = json.loads(manifest.read_text())
        valid = (isinstance(value, dict) and value.get("owner") == _OWNER
                 and value.get("schema_version") == _SCHEMA
                 and value.get("editor_path") == str(editor)
                 and value.get("backup_name") == _BACKUP
                 and isinstance(value.get("entrypoint"), str)
                 and "\0" not in value["entrypoint"]
                 and Path(value["entrypoint"]).is_absolute()
                 and isinstance(value.get("original_mode"), int)
                 and 0 <= value["original_mode"] <= 0o7777
                 and all(isinstance(value.get(k), str) and len(value[k]) == 64
                         and all(c in "0123456789abcdef" for c in value[k])
                         for k in ("original_sha256", "wrapper_sha256")))
        if not valid:
            raise ValueError("unrecognized integration record")
        return value
    except (ValueError, TypeError, KeyError) as exc:
        raise AppError("The scaling integration record is invalid. The Editor was left untouched.") from exc


@contextmanager
def _lock(editor: Path) -> Iterator[None]:
    # A directory flock adds no lock file to an Editor installation. All calls
    # lock the same directory inode, including across an executable replacement.
    fd = os.open(editor.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def _answer(editor: Path, state: str, detail: str, *, enabled: bool = False,
            can_enable: bool = False, can_disable: bool = False) -> dict:
    return {"editor_path": str(editor), "enabled": enabled, "state": state,
            "detail": detail, "can_enable": can_enable, "can_disable": can_disable}


def _status(editor: Path, backup: Path, manifest: Path) -> dict:
    if not _exists(manifest):
        if _exists(backup):
            return _answer(editor, "conflict", "An unrecognized Editor backup already exists. No files will be replaced.")
        if not _regular(editor) or not os.access(editor, os.X_OK):
            return _answer(editor, "error", "The Unity Editor executable is missing or is not executable.")
        with editor.open("rb") as stream:
            if stream.read(4) != b"\x7fELF":
                return _answer(editor, "conflict", "This executable is already a launcher or is not a Linux Editor. No files will be replaced.")
        writable = os.access(editor.parent, os.W_OK | os.X_OK)
        return _answer(editor, "disabled", "Uses the original Unity Editor launcher." if writable else "The Editor installation is not writable by your user.", can_enable=writable)
    try:
        record = _read_manifest(editor, manifest)
    except AppError as exc:
        return _answer(editor, "conflict", str(exc))
    if not _exists(backup) and _regular(editor) and _digest(editor) == record["original_sha256"] and _mode(editor) == record["original_mode"]:
        return _answer(editor, "conflict", "An interrupted integration can be safely cleaned up with Disable.", can_disable=True)
    if not _regular(backup) or _digest(backup) != record["original_sha256"] or _mode(backup) != record["original_mode"]:
        return _answer(editor, "conflict", "The original Editor backup is missing or has changed. Automatic restore is disabled.")
    if not _exists(editor):
        return _answer(editor, "conflict", "An interrupted integration can be safely restored with Disable.", can_disable=True)
    if not _regular(editor):
        return _answer(editor, "conflict", "The Editor executable was replaced by another file type. No files will be replaced.")
    digest = _digest(editor)
    if digest == record["wrapper_sha256"] and _mode(editor) == record["original_mode"]:
        return _answer(editor, "enabled", "Unity Hub and desktop launches use this Editor's scaling settings.", enabled=True, can_disable=True)
    if digest == record["original_sha256"] and _mode(editor) == record["original_mode"]:
        return _answer(editor, "conflict", "An interrupted integration can be safely cleaned up with Disable.", can_disable=True)
    return _answer(editor, "conflict", "The Editor launcher has changed, possibly after an update. No files will be replaced.")


def status(editor_path: str | Path) -> dict:
    """Read integration ownership and content; never start an Editor or write files."""
    editor = Path(editor_path).expanduser().absolute()
    try:
        editor, backup, manifest = _paths(editor_path)
        with _lock(editor):
            return _status(editor, backup, manifest)
    except (OSError, AppError) as exc:
        return _answer(editor, "error", str(exc))


def original_executable(editor_path: str | Path) -> str | None:
    """Resolve an active, verified wrapper for direct app launches.

    A conflicting installation raises instead of returning the wrapper, which
    would otherwise recursively invoke the runner. A plain native Editor has
    no override and returns None.
    """
    try:
        editor, backup, manifest = _paths(editor_path)
        with _lock(editor):
            current = _status(editor, backup, manifest)
            if current["enabled"]:
                return str(backup)
            if current["state"] == "disabled":
                return None
            raise AppError(current["detail"])
    except AppError:
        raise
    except OSError as exc:
        raise AppError(f"Could not verify the Editor integration: {exc}") from exc


def _wrapper(editor: Path, backup: Path, entrypoint: Path) -> bytes:
    # This launcher intentionally has no dependency on the Unity Scale package.
    # Removing the app leaves the installed Editor usable through its backup.
    source = f'''#!/usr/bin/python3
# {_MARKER}: {_OWNER} v{_SCHEMA}
import os
import sys

# Unity Hub is distributed as a Flatpak that runs this Editor inside its own
# sandbox, where Xwayland and the bridge's glibc are unavailable. The Editor and
# its display bridge must run on the host, so leave the sandbox first.
if (os.path.exists("/.flatpak-info")
        and os.environ.get("UNITY_SCALE_DISABLE_SANDBOX_HOP") != "1"):
    try:
        os.execvp("flatpak-spawn", ["flatpak-spawn", "--host",
                                     sys.executable or "/usr/bin/python3",
                                     os.path.abspath(__file__), *sys.argv[1:]])
    except OSError:
        pass  # No host spawn available: keep the current behavior.

editor = {str(editor)!r}
original = {str(backup)!r}
entrypoint = {str(entrypoint)!r}
arguments = sys.argv[1:]
# Utility and licensing invocations must remain completely unscaled.
bypass = {{"-batchmode", "-nographics", "-version", "--version", "-help", "--help",
          "-returnlicense", "-manuallicensefile", "-createmanualactivationfile"}}
use_app = not any(arg.lower() in bypass for arg in arguments)
# A scaled Editor or the pre-existing audit launcher already owns its bridge.
# Its child Editors must share that display, rather than create a nested bridge.
use_app = use_app and os.environ.get("UNITY_SCALE_ACTIVE") != "1"
use_app = use_app and os.environ.get("SCALING_AUDIT_VARIANT") != "launcher-supersampling"
use_app = use_app and os.path.isfile(entrypoint) and os.access(entrypoint, os.X_OK)
if use_app:
    command = [entrypoint, "run", "--editor", editor, "--real-executable", original, "--", *arguments]
    try:
        os.execv(entrypoint, command)
    except OSError:
        pass  # App removed between the existence check and exec: use Unity.
try:
    os.execv(original, [editor, *arguments])
except OSError as error:
    print("Unity Scale: cannot start the preserved Editor: " + str(error), file=sys.stderr)
    sys.exit(126)
'''
    return source.encode()


def enable(editor_path: str | Path, *, entrypoint: str | Path | None = None) -> dict:
    """Install an owned launcher only when the original and all paths are safe."""
    try:
        editor, backup, manifest = _paths(editor_path)
        entry = Path(entrypoint).expanduser().absolute() if entrypoint else ROOT / "bin/unity-scale"
        if entry == editor or entry == backup:
            raise AppError("The scaling app entry point cannot be the Editor executable.")
        with _lock(editor):
            current = _status(editor, backup, manifest)
            if current["enabled"]:
                existing = _read_manifest(editor, manifest)
                if entrypoint is not None and str(entry) != existing["entrypoint"]:
                    raise AppError("This Editor is enabled through another installation of this app. Disable it before enabling it through this installation.")
                fresh = _wrapper(editor, backup, Path(existing["entrypoint"]))
                if hashlib.sha256(fresh).hexdigest() != existing["wrapper_sha256"]:
                    raise AppError("This Editor uses an older scaling launcher. Disable it and enable it again to update the launcher safely.")
                return current
            if not current["can_enable"]:
                raise AppError(current["detail"])
            if not entry.is_file() or not os.access(entry, os.X_OK):
                raise AppError("Install this app before enabling Unity Hub integration.")
            original_stat = editor.stat()
            content = _wrapper(editor, backup, entry)
            record = {"owner": _OWNER, "schema_version": _SCHEMA,
                      "editor_path": str(editor), "backup_name": _BACKUP,
                      "entrypoint": str(entry), "original_sha256": _digest(editor),
                      "original_mode": stat.S_IMODE(original_stat.st_mode),
                      "wrapper_sha256": hashlib.sha256(content).hexdigest()}
            # Register before any installation mutation, so uninstall can find
            # even an interrupted operation. The sibling record is authoritative.
            _write_json(_record_path(editor), record)
            temporary = None
            manifest_created = backup_created = False
            try:
                _write_json(manifest, record, exclusive=True)
                manifest_created = True
                os.link(editor, backup, follow_symlinks=False)
                backup_created = True
                fd, temporary = tempfile.mkstemp(prefix=".Unity.unity-scale-", dir=editor.parent)
                with os.fdopen(fd, "wb") as stream:
                    stream.write(content)
                    stream.flush()
                    os.fchmod(stream.fileno(), record["original_mode"])
                    os.fsync(stream.fileno())
                # Refuse a concurrently replaced or edited original, including
                # noncooperating updaters that do not honor our directory lock.
                latest = editor.stat()
                if ((latest.st_dev, latest.st_ino) != (original_stat.st_dev, original_stat.st_ino)
                        or _digest(editor) != record["original_sha256"]
                        or _mode(editor) != record["original_mode"]):
                    raise AppError("The Editor changed while enabling integration. Try again after its update finishes.")
                os.replace(temporary, editor)
                temporary = None
                directory_fd = os.open(editor.parent, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(directory_fd)
                finally:
                    os.close(directory_fd)
            except BaseException:
                if temporary:
                    Path(temporary).unlink(missing_ok=True)
                # A failed pre-replacement attempt never overwrites the Editor.
                if backup_created and _regular(editor) and _digest(editor) == record["original_sha256"]:
                    backup.unlink(missing_ok=True)
                    if manifest_created:
                        manifest.unlink(missing_ok=True)
                    _record_path(editor).unlink(missing_ok=True)
                elif not backup_created and manifest_created:
                    manifest.unlink(missing_ok=True)
                    _record_path(editor).unlink(missing_ok=True)
                raise
            return _status(editor, backup, manifest)
    except AppError:
        raise
    except OSError as exc:
        raise AppError(f"Could not enable Unity Hub integration: {exc}. Existing Editor files were preserved.") from exc


def disable(editor_path: str | Path) -> dict:
    """Restore only an unchanged original behind our own unchanged launcher."""
    try:
        editor, backup, manifest = _paths(editor_path)
        with _lock(editor):
            current = _status(editor, backup, manifest)
            if current["state"] == "disabled":
                _record_path(editor).unlink(missing_ok=True)
                return current
            if not current["can_disable"]:
                raise AppError(current["detail"])
            # _status checked backup hash/mode plus owned wrapper or an
            # interrupted installation; an update is never overwritten.
            record = _read_manifest(editor, manifest)
            if _exists(editor) and _digest(editor) == record["original_sha256"]:
                backup.unlink(missing_ok=True)
            else:
                os.replace(backup, editor)
            manifest.unlink()
            _record_path(editor).unlink(missing_ok=True)
            return _status(editor, backup, manifest)
    except AppError:
        raise
    except OSError as exc:
        raise AppError(f"Could not restore the original Editor: {exc}. Check the installation's permissions.") from exc


def integration_records() -> list[dict]:
    """Return valid user-owned registry records for the application uninstaller."""
    records = []
    directory = Store().state_dir / "integrations"
    for path in directory.glob("*.json"):
        try:
            if not _regular(path) or path.stat().st_size > 16384:
                continue
            value = json.loads(path.read_text())
            if (isinstance(value, dict) and value.get("owner") == _OWNER
                    and isinstance(value.get("editor_path"), str)
                    and path.name == editor_key(value["editor_path"]) + ".json"):
                records.append(value)
        except (OSError, ValueError, TypeError):
            continue
    return records
