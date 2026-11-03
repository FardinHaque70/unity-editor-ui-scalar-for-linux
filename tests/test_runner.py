from __future__ import annotations

import contextlib
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest import mock

from unity_scale import core, integration, runner

_resolve_original = integration.original_executable


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.project = self.root / "Project with spaces"
        self.project.mkdir()
        self.editor = self.root / "Unity"
        self.helper = self.root / "bridge"
        self.capture = self.root / "editor.json"
        self.helper_capture = self.root / "helper.json"
        self.stopped = self.root / "helper.stopped"
        self.release = self.root / "release"
        self.editor.write_text("""#!/usr/bin/env python3
import json,os,pathlib,sys,time
pathlib.Path(os.environ['TEST_EDITOR_CAPTURE']).write_text(json.dumps({
 'argv':sys.argv[1:],'env':dict(os.environ),'pid':os.getpid(),
 'sid':os.getsid(0),'cwd':os.getcwd()}))
if os.environ.get('TEST_HOLD_EDITOR'):
 while not pathlib.Path(os.environ['TEST_RELEASE']).exists():time.sleep(.01)
raise SystemExit(int(os.environ.get('TEST_EXIT_CODE','0')))
""")
        self.helper.write_text("""#!/usr/bin/env python3
import json,os,pathlib,signal,sys,time
pathlib.Path(os.environ['TEST_HELPER_CAPTURE']).write_text(json.dumps({
 'argv':sys.argv[1:],'env':dict(os.environ),'pid':os.getpid(),'sid':os.getsid(0)}))
def stop(*args):
 pathlib.Path(os.environ['TEST_HELPER_STOPPED']).write_text('stopped')
 raise SystemExit(0)
signal.signal(signal.SIGTERM,stop)
while True:time.sleep(.01)
""")
        self.editor.chmod(0o755)
        self.helper.chmod(0o755)
        self.profile = {"ui_scale_percent": 150, "follow_desktop": False,
                        "render_scale": 2, "cursor_theme": "Breeze_Light", "cursor_size": 36}
        self.store = SimpleNamespace(state_dir=self.root / "state", get_profile=mock.Mock(return_value=self.profile))
        self.env = {"PATH": os.environ["PATH"], "WAYLAND_DISPLAY": "wayland-test",
                    "DISPLAY": ":0", "XDG_CURRENT_DESKTOP": "KDE", "XDG_SESSION_TYPE": "wayland",
                    "XDG_RUNTIME_DIR": str(self.root / "runtime"),
                    "XDG_CONFIG_HOME": str(self.root / "config"),
                    "XDG_STATE_HOME": str(self.root / "xdg-state"),
                    "XDG_DATA_HOME": str(self.root / "xdg-data"),
                    "TEST_EDITOR_CAPTURE": str(self.capture),
                    "TEST_HELPER_CAPTURE": str(self.helper_capture),
                    "TEST_HELPER_STOPPED": str(self.stopped), "TEST_RELEASE": str(self.release)}
        for patcher in (
            mock.patch.dict(os.environ, self.env, clear=True),
            mock.patch.object(core, "Store", return_value=self.store),
            mock.patch.object(core, "helper_path", return_value=self.helper),
            mock.patch.object(core, "environment_status", return_value={
                "supported": True, "desktop_scale_percent": 145, "summary": "ready"}),
            mock.patch.object(integration, "original_executable", return_value=None),
            mock.patch.object(runner, "_HANDOFF_GRACE_SECONDS", .03),
            mock.patch.object(runner, "_PROCESS_POLL_SECONDS", .01),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def wait_file(self, path):
        deadline = time.monotonic() + 4
        while not path.exists():
            if time.monotonic() > deadline:
                self.fail(f"Timed out waiting for {path}")
            time.sleep(.01)
        return json.loads(path.read_text())

    def ready(self, process, display, density, log_path):
        self.wait_file(self.helper_capture)
        return density

    def test_plan_preserves_hub_argv_identity_and_real_binary(self):
        original = self.root / "Original Unity"
        args = ["-projectPath", str(self.project), "-hubSessionId", "opaque value", "-custom", "x=y"]
        plan = runner.launch_plan(str(original), extra_args=args, real_executable=str(self.editor))
        self.assertEqual(plan["editor"], str(original))
        self.assertEqual(plan["editor_command"], [str(self.editor), *args])
        self.assertEqual(plan["project"], str(self.project))
        self.store.get_profile.assert_called_once_with(str(original))
        self.assertAlmostEqual(plan["ui_scaling"]["multiplier"], 150 / 145)
        self.assertAlmostEqual(plan["window_coordinate_scale"], 2 / (150 / 145))
        self.assertEqual(plan["helper_environment"]["XCURSOR_SIZE"], "72")
        self.assertEqual(plan["editor_environment"]["GDK_DPI_SCALE"], "0.5")
        self.assertNotIn("GDK_BACKEND", plan["helper_environment"])

    def test_bridge_log_level_can_be_raised_for_diagnostics(self):
        with mock.patch.dict(os.environ, {"UNITY_SCALE_BRIDGE_LOG": "debug"}):
            plan = runner.launch_plan(str(self.root / "Original Unity"), real_executable=str(self.editor))
        self.assertEqual(plan["helper_environment"]["RUST_LOG"], "debug")
        self.assertEqual(runner.launch_plan(str(self.root / "Original Unity"), real_executable=str(self.editor))["helper_environment"]["RUST_LOG"], "warn")

    def test_no_project_required_and_dry_run_has_no_files_or_processes(self):
        with mock.patch.object(subprocess, "Popen") as popen, contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(runner.run_editor(str(self.editor), dry_run=True), 0)
        plan = json.loads(output.getvalue())
        self.assertIsNone(plan["project"])
        self.assertEqual(plan["editor_command"], [str(self.editor)])
        self.assertFalse(self.store.state_dir.exists())
        self.assertFalse((self.root / "runtime").exists())
        popen.assert_not_called()

    def test_bypass_is_exact_and_does_not_need_profile_or_wayland(self):
        os.environ.pop("WAYLAND_DISPLAY")
        os.environ["GDK_SCALE"] = "7"
        with mock.patch.object(integration, "original_executable", side_effect=AssertionError("Must bypass resolution")):
            for flag in ("-version", "-batchmode", "-nographics"):
                args = [flag, "-custom", "with spaces"]
                self.assertEqual(runner.run_editor(str(self.editor), extra_args=args), 0)
                capture = json.loads(self.capture.read_text())
                self.assertEqual(capture["argv"], args)
                self.assertEqual(capture["env"]["GDK_SCALE"], "7")
        self.store.get_profile.assert_not_called()
        self.assertFalse(self.helper_capture.exists())
        self.assertEqual(runner.launch_plan(str(self.editor), extra_args=["/projects/-batchmode"])["mode"], "scaled")

    def test_secret_arguments_forwarded_but_redacted_in_dry_run(self):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            runner.run_editor(str(self.editor), extra_args=["-password", "secret", "-serial=private"], dry_run=True)
        self.assertNotIn("secret", output.getvalue())
        self.assertNotIn("private", output.getvalue())
        self.assertIn("<redacted>", output.getvalue())

    def test_fake_children_survive_caller_return_and_cleanup_after_editor_exit(self):
        os.environ["TEST_HOLD_EDITOR"] = "1"
        os.environ["TEST_EXIT_CODE"] = "7"
        os.environ["GDK_SCALE"] = "99"
        os.environ["SDL_VIDEODRIVER"] = "wayland"
        os.environ["UNITY_SATELLITE_INTEGER_RENDER"] = "1"
        result, errors = [], []

        def supervise():
            try:
                with mock.patch.object(runner, "_wait_for_bridge", side_effect=self.ready):
                    result.append(runner.run_editor(str(self.editor), project=str(self.project),
                                                   extra_args=["-hubSessionId", "keep this"]))
            except BaseException as error:
                errors.append(error)

        thread = threading.Thread(target=supervise)
        thread.start()
        try:
            editor = self.wait_file(self.capture)
            helper = self.wait_file(self.helper_capture)
            self.assertTrue(thread.is_alive())
            self.assertEqual(editor["sid"], editor["pid"])
            self.assertEqual(helper["sid"], helper["pid"])
            self.assertEqual(editor["env"]["GDK_SCALE"], "2")
            self.assertEqual(editor["env"]["GDK_BACKEND"], "x11")
            self.assertEqual(editor["env"]["GDK_DPI_SCALE"], "0.5")
            self.assertEqual(editor["env"]["UNITY_SCALE_ACTIVE"], "1")
            self.assertNotIn("SDL_VIDEODRIVER", editor["env"])
            self.assertNotIn("UNITY_SATELLITE_INTEGER_RENDER", helper["env"])
            self.assertEqual(helper["env"]["UNITY_SATELLITE_RENDER_SCALE"], "2")
            self.assertEqual(editor["env"]["XCURSOR_SIZE"], helper["env"]["XCURSOR_SIZE"])
            self.assertEqual(editor["argv"][:4], ["-hubSessionId", "keep this", "-projectPath", str(self.project)])
            self.assertNotEqual(editor["cwd"], str(self.project))
            self.assertEqual(list(self.project.iterdir()), [])
            self.assertFalse(self.stopped.exists())
        finally:
            self.release.touch()
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(result, [7])
        self.assertTrue(self.stopped.exists())
        metadata = json.loads(next((self.store.state_dir / "runs").glob("*/launch.json")).read_text())
        self.assertEqual(metadata["status"], "exited")
        self.assertEqual(metadata["exit_code"], 7)
        self.assertEqual(metadata["editor_pid"], editor["pid"])
        self.assertEqual(os.environ["GDK_SCALE"], "99")
        # Both project/display locks have been released after clean shutdown.
        lock = runner._project_lock(runner._runtime_directory(), str(self.project))
        lock.close()

    def test_failed_bridge_does_not_launch_editor_and_releases_project_lock(self):
        with mock.patch.object(runner, "_wait_for_bridge", side_effect=core.AppError("startup failed")):
            with self.assertRaisesRegex(core.AppError, "startup failed"):
                runner.run_editor(str(self.editor), project=str(self.project))
        self.assertFalse(self.capture.exists())
        lock = runner._project_lock(runner._runtime_directory(), str(self.project))
        lock.close()
        metadata = json.loads(next((self.store.state_dir / "runs").glob("*/launch.json")).read_text())
        self.assertEqual(metadata["status"], "failed")

    def test_project_lock_coexists_with_existing_audit_namespace(self):
        runtime = runner._runtime_directory()
        key = hashlib.sha256(str(self.project).encode()).hexdigest()[:16]
        with (runtime / f"project-{key}.lock").open("a+") as existing:
            fcntl.flock(existing, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(core.AppError, "already running"):
                runner.run_editor(str(self.editor), project=str(self.project))

    def test_existing_logfile_and_missing_project_argument_are_forwarded(self):
        args = ["-logFile", "-", "-projectPath"]
        with mock.patch.object(runner, "_wait_for_bridge", side_effect=self.ready):
            self.assertEqual(runner.run_editor(str(self.editor), extra_args=args), 0)
        self.assertEqual(json.loads(self.capture.read_text())["argv"], args)

    def test_detached_supervisor_and_children_survive_hangup_then_exit_cleanly(self):
        # Run a real supervisor in its own session, as the GUI does, and close
        # its controlling caller channel with SIGHUP while the fake Editor runs.
        config = {"editor": str(self.editor), "helper": str(self.helper),
                  "state": str(self.store.state_dir), "profile": self.profile,
                  "helper_capture": str(self.helper_capture)}
        script = """
import json,sys,time
from pathlib import Path
from types import SimpleNamespace
from unity_scale import core,runner
from unity_scale import integration
c=json.loads(sys.argv[1])
integration.original_executable=lambda _:None
runner._HANDOFF_GRACE_SECONDS=.03
runner._PROCESS_POLL_SECONDS=.01
core.Store=lambda:SimpleNamespace(state_dir=Path(c['state']),get_profile=lambda _:c['profile'])
core.helper_path=lambda:Path(c['helper'])
core.environment_status=lambda:{'supported':True,'desktop_scale_percent':145}
def ready(process,display,density,log):
 deadline=time.monotonic()+3
 while not Path(c['helper_capture']).exists():
  if time.monotonic()>deadline:raise RuntimeError('fake helper startup timeout')
  time.sleep(.01)
 return density
runner._wait_for_bridge=ready
raise SystemExit(runner.run_editor(c['editor']))
"""
        env = os.environ.copy()
        env["TEST_HOLD_EDITOR"] = "1"
        with subprocess.Popen([sys.executable, "-c", script, json.dumps(config)], env=env,
                              start_new_session=True, stdout=subprocess.DEVNULL,
                              stderr=subprocess.PIPE, text=True) as supervisor:
            try:
                editor = self.wait_file(self.capture)
                helper = self.wait_file(self.helper_capture)
                os.kill(supervisor.pid, signal.SIGHUP)
                time.sleep(.05)
                self.assertIsNone(supervisor.poll())
                os.kill(editor["pid"], 0)
                os.kill(helper["pid"], 0)
                self.assertFalse(self.stopped.exists())
            finally:
                self.release.touch()
                _, error = supervisor.communicate(timeout=5)
        self.assertEqual(supervisor.returncode, 0, error)
        self.assertTrue(self.stopped.exists())

    def test_gui_run_of_integrated_editor_uses_verified_original_without_recursion(self):
        # Real integration of an isolated ELF fixture, followed by the same
        # original-path API call made by the GUI (no real-executable override).
        shutil.copy2(shutil.which("true"), self.editor)
        try:
            enabled = integration.enable(str(self.editor), entrypoint=shutil.which("false"))
            self.assertTrue(enabled["enabled"])
            with mock.patch.object(integration, "original_executable", side_effect=_resolve_original), \
                 mock.patch.object(runner, "_wait_for_bridge", side_effect=self.ready):
                self.assertEqual(runner.run_editor(str(self.editor)), 0)
            metadata = json.loads(next((self.store.state_dir / "runs").glob("*/launch.json")).read_text())
            self.assertEqual(metadata["editor"], str(self.editor))
            self.assertEqual(metadata["real_executable"], str(self.editor.with_name("Unity.unity-scale-original")))
            self.assertEqual(metadata["editor_command"][0], metadata["real_executable"])
            self.store.get_profile.assert_called_with(str(self.editor))
        finally:
            integration.disable(str(self.editor))

    def test_native_fork_exec_replacement_keeps_display_until_last_editor_exits(self):
        replacement = self.root / "replacement.py"
        replacement_capture = self.root / "replacement.json"
        replacement_done = self.root / "replacement.done"
        replacement.write_text("""import json,os,pathlib,time
pathlib.Path(os.environ['TEST_REPLACEMENT_CAPTURE']).write_text(json.dumps({'pid':os.getpid()}))
for _ in range(20):time.sleep(.02)
assert not pathlib.Path(os.environ['TEST_HELPER_STOPPED']).exists(), 'display closed during relaunch'
pathlib.Path(os.environ['TEST_REPLACEMENT_DONE']).write_text('finished')
""")
        self.editor.write_text("""#!/usr/bin/env python3
import os,sys
if os.fork()==0:
 os.execv(sys.executable,[sys.executable,os.environ['TEST_REPLACEMENT_SCRIPT']])
raise SystemExit(0)
""")
        os.environ.update(TEST_REPLACEMENT_SCRIPT=str(replacement),
                          TEST_REPLACEMENT_CAPTURE=str(replacement_capture),
                          TEST_REPLACEMENT_DONE=str(replacement_done))
        interpreter = Path(shutil.which("python3")).stat()
        identity = (interpreter.st_dev, interpreter.st_ino)
        # Same executable inode on another DISPLAY must not retain this bridge.
        other_env = os.environ.copy()
        other_env["DISPLAY"] = ":unrelated"
        other = subprocess.Popen([shutil.which("python3"), "-c", "import time;time.sleep(10)"], env=other_env)
        try:
            with mock.patch.object(runner, "_wait_for_bridge", side_effect=self.ready), \
                 mock.patch.object(runner, "_executable_identity", return_value=identity), \
                 mock.patch.object(runner, "_HANDOFF_GRACE_SECONDS", .2):
                self.assertEqual(runner.run_editor(str(self.editor)), 0)
            self.assertTrue(replacement_done.exists())
            self.assertTrue(self.stopped.exists())
            self.assertIsNone(other.poll(), "Unrelated Editor fixture should remain untouched")
            metadata = json.loads(next((self.store.state_dir / "runs").glob("*/launch.json")).read_text())
            replacement_pid = json.loads(replacement_capture.read_text())["pid"]
            self.assertIn(replacement_pid, metadata["handoff_seen_pids"])
            self.assertNotIn(other.pid, metadata["handoff_seen_pids"])
            self.assertEqual(metadata["handoff_editor_pids"], [])
            self.assertEqual(metadata["initial_editor_exit_code"], 0)
            self.assertFalse(metadata["handoff_exit_code_available"])
        finally:
            other.terminate()
            other.wait(timeout=3)

    def test_hub_lowercase_projectpath_is_recognized(self):
        self.assertEqual(runner._project_from_args(["-projectpath", "/tmp/project"]), "/tmp/project")
        self.assertEqual(runner._project_from_args(["-projectpath=/tmp/project"]), "/tmp/project")
        self.assertIsNone(runner._project_from_args(["-projectPath"]))


if __name__ == "__main__":
    unittest.main()
