"""Supervise one Editor and its private, fixed-density XWayland bridge.

The GUI starts this module's CLI in a detached session. Closing the GUI does not
end the supervisor, Editor, or bridge. All launcher-owned files live outside
Unity projects; the audit launcher's runtime lock namespace is retained.
"""
from __future__ import annotations

import configparser
import ctypes
import ctypes.util
import datetime as dt
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import uuid

from . import core


_DIRECT_FLAGS = frozenset({"-version", "-batchmode", "-nographics"})
_SECRET_FLAGS = frozenset({"-password", "-serial", "-accessToken", "-proxyPassword"})
_HANDOFF_GRACE_SECONDS = 5.0
_PROCESS_POLL_SECONDS = .1


def _executable(value: str) -> Path:
    path = Path(value).expanduser().resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
        raise core.AppError(f"Editor executable not found or not executable: {path}")
    return path


def _project_from_args(args: list[str]) -> str | None:
    # Unity Hub passes -projectpath in lower case; Unity itself is insensitive.
    for index, value in enumerate(args):
        lowered = value.lower()
        if lowered == "-projectpath" and index + 1 < len(args):
            return str(Path(args[index + 1]).expanduser().resolve())
        if lowered.startswith("-projectpath="):
            return str(Path(value.partition("=")[2]).expanduser().resolve())
    return None


def cursor_preferences(env: dict[str, str], theme_override=None, size_override=None):
    """Read logical desktop cursor preferences, before multiplying by density B."""
    config_home = Path(env.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
    desktop = env.get("XDG_CURRENT_DESKTOP", "").lower()
    theme, size, source = None, None, "GTK/default cursor preferences"
    if "kde" in desktop:
        settings = configparser.ConfigParser(interpolation=None, strict=False)
        settings.read(config_home / "kcminputrc")
        if settings.has_section("Mouse"):
            theme = settings.get("Mouse", "cursorTheme", fallback=None)
            try:
                size = settings.getint("Mouse", "cursorSize", fallback=None)
            except ValueError:
                pass
            source = "KDE logical cursor preferences"
    if "gnome" in desktop and shutil.which("gsettings"):
        for key in ("cursor-theme", "cursor-size"):
            try:
                value = subprocess.check_output(
                    ["gsettings", "get", "org.gnome.desktop.interface", key], env=env,
                    text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
                if key == "cursor-theme":
                    theme = value.strip("'\"")
                else:
                    size = int(value)
                source = "GNOME logical cursor preferences"
            except (OSError, ValueError, subprocess.SubprocessError):
                pass
    gtk = configparser.ConfigParser(interpolation=None, strict=False)
    gtk.read(config_home / "gtk-3.0/settings.ini")
    if not theme:
        theme = env.get("XCURSOR_THEME") or gtk.get(
            "Settings", "gtk-cursor-theme-name", fallback="Adwaita")
    if not size or size <= 0:
        try:
            size = int(env.get("XCURSOR_SIZE") or gtk.get(
                "Settings", "gtk-cursor-theme-size", fallback="24"))
        except ValueError:
            size = 24
    theme = theme_override or theme
    size = size_override if size_override is not None else size
    if not isinstance(theme, str) or not theme or any(c in theme for c in "\r\n\0"):
        raise core.AppError("Invalid cursor theme name.")
    if isinstance(size, bool) or not isinstance(size, int) or not 1 <= size <= 256:
        raise core.AppError("Logical cursor size must be an integer from 1 to 256.")
    return theme, size, source


def launch_plan(editor_path: str, project=None, extra_args=None, real_executable=None) -> dict:
    """Return a non-mutating launch plan, retaining the original profile identity."""
    original = str(Path(editor_path).expanduser().resolve())
    args = list(extra_args or [])
    if any(not isinstance(value, str) or "\0" in value for value in args):
        raise core.AppError("Unity arguments must be strings without NUL characters.")
    argument_project = _project_from_args(args)
    if project is not None and argument_project is None:
        project = str(Path(project).expanduser().resolve())
        args += ["-projectPath", project]
    else:
        project = argument_project
    bypass = sorted(_DIRECT_FLAGS.intersection(args))
    if real_executable is None and not bypass:
        # GUI launches use the public Editor identity, which may now be our
        # integration wrapper. Resolve only a verified, owned native backup.
        # Explicit headless utilities may enter that wrapper, whose own direct
        # bypass preserves them without consulting settings or starting a bridge.
        from . import integration
        real_executable = integration.original_executable(original)
    executable = _executable(real_executable or original)
    plan = {
        "editor": original, "real_executable": str(executable), "project": project,
        "editor_command": [str(executable), *args], "mode": "direct" if bypass else "scaled",
        "bypass_flags": bypass, "desktop_settings_changed": False,
    }
    if bypass:
        return plan

    profile = core.Store().get_profile(original)
    density = profile.get("render_scale", 2)
    if isinstance(density, bool) or not isinstance(density, int) or density not in range(1, 5):
        raise core.AppError("Render density must be an integer from 1 to 4.")
    status = core.environment_status()
    desktop = profile.get("desktop_scale_percent")
    if desktop is None:
        desktop = status.get("desktop_scale_percent")
    if desktop is None:
        desktop = os.environ.get("UNITY_DESKTOP_SCALE_PERCENT")
    if desktop is not None:
        desktop = float(desktop)
        if not math.isfinite(desktop) or not 50 <= desktop <= 500:
            raise core.AppError("Desktop scale reference must be between 50 and 500 percent.")
    follow = bool(profile.get("follow_desktop", False))
    requested = desktop if follow else float(profile.get("ui_scale_percent", 150))
    minimum = getattr(core, "MIN_UI_SCALE", 75)
    maximum = getattr(core, "MAX_UI_SCALE", 250)
    if not follow and (not math.isfinite(requested) or not minimum <= requested <= maximum):
        raise core.AppError(f"Unity UI scale must be between {minimum} and {maximum} percent.")
    if not follow and desktop is None:
        raise core.AppError("Cannot detect desktop scale. Set this Editor's desktop scale reference first.")
    multiplier = 1.0 if follow else requested / desktop
    theme, size, source = cursor_preferences(
        os.environ, profile.get("cursor_theme"), profile.get("cursor_size"))
    if size * density > 128:
        raise core.AppError(
            "GTK3 supports cursor sizes up to 128 backing pixels. Lower the render density "
            "or logical cursor size so their product is at most 128.")
    cursor_env = {"XCURSOR_THEME": theme, "XCURSOR_SIZE": str(size * density)}
    plan.update({
        "helper": str(core.helper_path()), "render_scale": density,
        "ui_scaling": {"ui_scale_percent": requested, "desktop_scale_percent": desktop,
                       "multiplier": multiplier, "follow_desktop": follow},
        "window_coordinate_scale": density / multiplier,
        "cursor": {"theme": theme, "logical_size": size,
                   "backing_size": size * density, "source": source},
        "helper_environment": {**cursor_env, "UNITY_SATELLITE_RENDER_SCALE": str(density),
                               "UNITY_SATELLITE_UI_MULTIPLIER": format(multiplier, ".16g"),
                               "RUST_LOG": os.environ.get("UNITY_SCALE_BRIDGE_LOG", "warn")},
        "editor_environment": {**cursor_env, "UNITY_SCALE_ACTIVE": "1",
                               "GDK_BACKEND": "x11", "GDK_SCALE": str(density),
                               "GDK_DPI_SCALE": format(1.0 / density, ".8g")},
        "environment_status": status,
    })
    return plan


class XDisplay:
    def __init__(self, name: str):
        self.lib = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
        self.lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.lib.XOpenDisplay.restype = ctypes.c_void_p
        self.lib.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.lib.XResourceManagerString.argtypes = [ctypes.c_void_p]
        self.lib.XResourceManagerString.restype = ctypes.c_char_p
        self.ptr = self.lib.XOpenDisplay(name.encode())
        if not self.ptr:
            raise core.AppError(f"Cannot connect to isolated display {name}")

    def render_scale(self) -> int:
        resources = self.lib.XResourceManagerString(self.ptr) or b""
        match = re.search(rb"(?m)^Xft\.dpi:\s*([0-9.]+)", resources)
        if not match:
            raise core.AppError("The isolated display did not advertise its font/render DPI.")
        ratio = float(match[1]) / 96.0
        factor = round(ratio)
        if not 1 <= factor <= 4 or abs(ratio - factor) > .02:
            raise core.AppError(f"Expected integer render density; got DPI {float(match[1])}.")
        return factor

    def close(self):
        if self.ptr:
            self.lib.XCloseDisplay(self.ptr)
            self.ptr = None


def _wait_for_bridge(process, display: str, density: int, log_path: Path) -> int:
    deadline = time.monotonic() + 20
    last_error = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise core.AppError(f"The bridge exited. See {log_path}")
        connection = None
        try:
            connection = XDisplay(display)
            factor = connection.render_scale()
            if factor != density:
                raise core.AppError(f"Bridge advertised density {factor}; requested {density}.")
            return factor
        except core.AppError as error:
            last_error = error
        finally:
            if connection:
                connection.close()
        time.sleep(.1)
    raise core.AppError(f"Bridge startup timed out: {last_error}. See {log_path}")


def _stop_helper(process):
    if process is None or process.poll() is not None:
        return
    try:
        process.terminate()
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def _runtime_directory() -> Path:
    base = Path(os.environ.get("XDG_RUNTIME_DIR", "/tmp"))
    if not base.is_absolute():
        base = Path("/tmp")
    runtime = base / f"unity-scaling-{os.getuid()}"
    runtime.mkdir(mode=0o700, parents=True, exist_ok=True)
    if runtime.is_symlink() or runtime.stat().st_uid != os.getuid():
        raise core.AppError(f"The private display lock directory is not owned by this user: {runtime}")
    runtime.chmod(0o700)
    return runtime


def _project_lock(runtime: Path, project: str | None):
    if project is None:
        return None
    key = hashlib.sha256(project.encode()).hexdigest()[:16]
    handle = (runtime / f"project-{key}.lock").open("a+")
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        handle.close()
        raise core.AppError("This project's scaling launcher is already running.") from None
    return handle


def _display_lock(runtime: Path):
    for number in range(90, 120):
        candidate = (runtime / f"display-{number}.lock").open("a+")
        try:
            fcntl.flock(candidate, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            candidate.close()
            continue
        if Path(f"/tmp/.X11-unix/X{number}").exists() or Path(f"/tmp/.X{number}-lock").exists():
            candidate.close()
            continue
        return candidate, f":{number}"
    raise core.AppError("No unused isolated X display was found in :90 through :119.")


def _redacted_plan(plan: dict) -> dict:
    result = dict(plan)
    redact_next = False
    command = []
    for value in plan["editor_command"]:
        if redact_next:
            command.append("<redacted>")
            redact_next = False
        elif value.partition("=")[0] in _SECRET_FLAGS:
            if "=" in value:
                command.append(value.partition("=")[0] + "=<redacted>")
            else:
                command.append(value)
                redact_next = True
        else:
            command.append(value)
    result["editor_command"] = command
    return result


def _write_state(path: Path, state: dict):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(state, indent=2) + "\n")
    temporary.replace(path)


def _executable_identity(path: str) -> tuple[int, int]:
    stat = Path(path).stat()
    return stat.st_dev, stat.st_ino


def _editors_on_display(identity: tuple[int, int], display: str) -> list[int]:
    """Read only the identity/state/display needed to find native relaunches.

    A replacement need not remain our child after Unity's initial process exits.
    Matching the private display prevents another instance of the same Editor,
    licensing tools, or an unrelated interpreter fixture retaining this bridge.
    No process environment is returned, recorded, or printed.
    """
    result = []
    expected_display = b"DISPLAY=" + display.encode()
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            if proc.stat().st_uid != os.getuid():
                continue
            state = proc.joinpath("stat").read_bytes().rpartition(b")")[2].split()[0]
            if state == b"Z":
                continue
            stat = proc.joinpath("exe").stat()
            if (stat.st_dev, stat.st_ino) != identity:
                continue
            if expected_display in proc.joinpath("environ").read_bytes().split(b"\0"):
                result.append(int(proc.name))
        except (OSError, IndexError):
            # A process can exit or exec between any two reads.
            continue
    return sorted(result)


def _wait_for_handoffs(identity, display: str, metadata: dict, state_file: Path):
    deadline = time.monotonic() + _HANDOFF_GRACE_SECONDS
    seen = set()
    previous = None
    while True:
        try:
            current = _editors_on_display(identity, display)
            if current:
                deadline = time.monotonic() + _HANDOFF_GRACE_SECONDS
                seen.update(current)
                if current != previous:
                    metadata.update(status="handoff", handoff_editor_pids=current,
                                    handoff_seen_pids=sorted(seen), editor_pid=current[0],
                                    handoff_exit_code_available=False)
                    try:
                        _write_state(state_file, metadata)
                    except OSError as error:
                        print(f"Could not write Editor handoff status: {error}", file=sys.stderr)
            elif time.monotonic() >= deadline:
                if seen:
                    metadata["handoff_editor_pids"] = []
                return
            previous = current
            time.sleep(_PROCESS_POLL_SECONDS)
        except KeyboardInterrupt:
            # The replacement may also have unsaved work. Keep its display.
            continue


def run_editor(editor_path: str, project=None, extra_args=None, dry_run=False,
               real_executable=None) -> int:
    """Run a detached Editor, retaining its private display until it exits."""
    plan = launch_plan(editor_path, project, extra_args, real_executable)
    if dry_run:
        print(json.dumps(_redacted_plan(plan), indent=2))
        return 0
    if plan["mode"] == "direct":
        # Headless/version commands retain their exact argv and environment.
        return subprocess.call(plan["editor_command"], start_new_session=True)
    if not os.environ.get("WAYLAND_DISPLAY"):
        raise core.AppError(f"{core.APP_NAME} requires a Wayland desktop for graphical Editor launches.")
    if not plan["environment_status"].get("supported", False):
        raise core.AppError(plan["environment_status"].get("summary", "Scaling preflight did not pass."))
    helper_path = Path(plan["helper"])
    if not helper_path.is_file() or not os.access(helper_path, os.X_OK):
        raise core.AppError(f"Missing executable bridge: {helper_path}. See its source build instructions.")

    runtime = _runtime_directory()
    project_lock = _project_lock(runtime, plan["project"])
    display_lock = helper = editor = None
    native_identity = None
    run_dir = Path(core.Store().state_dir) / "runs" / (
        dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:8])
    run_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    metadata = {**_redacted_plan(plan), "status": "starting", "supervisor_pid": os.getpid(),
                "log_directory": str(run_dir), "started_at": dt.datetime.now(dt.timezone.utc).isoformat()}
    state_file = run_dir / "launch.json"
    old_hup = None
    if threading.current_thread() is threading.main_thread():
        old_hup = signal.signal(signal.SIGHUP, signal.SIG_IGN)
    try:
        display_lock, display = _display_lock(runtime)
        metadata["display"] = display
        _write_state(state_file, metadata)
        helper_env = os.environ.copy()
        helper_env.pop("UNITY_SATELLITE_INTEGER_RENDER", None)
        helper_env.update(plan["helper_environment"])
        editor_env = os.environ.copy()
        for key in ("GDK_SCALE", "GDK_DPI_SCALE", "SDL_VIDEODRIVER",
                    "UNITY_SATELLITE_INTEGER_RENDER", "UNITY_SATELLITE_RENDER_SCALE",
                    "UNITY_SATELLITE_UI_MULTIPLIER"):
            editor_env.pop(key, None)
        editor_env.update(plan["editor_environment"])
        editor_env["DISPLAY"] = display
        editor_env["UNITY_HOST_DISPLAY"] = os.environ.get("UNITY_HOST_DISPLAY", os.environ.get("DISPLAY", ""))
        scaling = plan["ui_scaling"]
        if scaling["desktop_scale_percent"] is not None:
            editor_env["UNITY_DESKTOP_SCALE_PERCENT"] = str(scaling["desktop_scale_percent"])
        if scaling["ui_scale_percent"] is not None:
            editor_env["UNITY_EDITOR_UI_SCALE_PERCENT"] = str(scaling["ui_scale_percent"])
        command = list(plan["editor_command"])
        if not any(value.lower() == "-logfile" or value.lower().startswith("-logfile=") for value in command[1:]):
            command += ["-logFile", str(run_dir / "Editor.log")]
        metadata["editor_command"] = _redacted_plan({"editor_command": command})["editor_command"]
        with (run_dir / "bridge.log").open("w") as bridge_log, (run_dir / "Editor.stdout.log").open("w") as editor_log:
            helper = subprocess.Popen([str(helper_path), display], env=helper_env,
                                      stdout=bridge_log, stderr=subprocess.STDOUT, start_new_session=True)
            metadata["helper_pid"] = helper.pid
            _write_state(state_file, metadata)
            _wait_for_bridge(helper, display, plan["render_scale"], run_dir / "bridge.log")
            native_identity = _executable_identity(plan["real_executable"])
            editor = subprocess.Popen(command, env=editor_env, cwd=Path(plan["real_executable"]).parent,
                                      stdout=editor_log, stderr=subprocess.STDOUT, start_new_session=True)
            metadata.update(status="running", editor_pid=editor.pid, initial_editor_pid=editor.pid)
            _write_state(state_file, metadata)
            print(f"Opening Unity. Logs: {run_dir}", flush=True)
            while True:
                try:
                    result = editor.wait()
                    metadata.update(status="exited", exit_code=result, initial_editor_exit_code=result)
                    return result
                except KeyboardInterrupt:
                    print("Editor left running. Close it normally when finished.", file=sys.stderr)
    except BaseException as error:
        metadata.update(status="failed", error=str(error))
        raise
    finally:
        # Even a launcher/logging error after spawn must not remove the display
        # underneath an Editor that may contain unsaved work.
        if editor is not None:
            while editor.poll() is None:
                try:
                    editor.wait()
                except KeyboardInterrupt:
                    continue
            if native_identity is not None and helper is not None and helper.poll() is None:
                failed = metadata.get("status") == "failed"
                _wait_for_handoffs(native_identity, display, metadata, state_file)
                metadata["status"] = "failed" if failed else "exited"
        try:
            _stop_helper(helper)
        finally:
            metadata["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
            try:
                _write_state(state_file, metadata)
            except OSError as error:
                print(f"Could not write final launch status: {error}", file=sys.stderr)
            finally:
                if display_lock:
                    display_lock.close()
                if project_lock:
                    project_lock.close()
                if old_hup is not None:
                    signal.signal(signal.SIGHUP, old_hup)
