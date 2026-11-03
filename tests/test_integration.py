"""Exercise interception with disposable native executables, never installed Unity."""
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from unity_scale import integration
from unity_scale.core import AppError, inspect_editor


class IntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="unity-scale-integration-")
        self.root = Path(self.temporary.name)
        self.environment = patch.dict(os.environ, {"XDG_STATE_HOME": str(self.root / "state"),
                                                    "UNITY_SCALE_DISABLE_SANDBOX_HOP": "1"})
        self.environment.start()
        self.editor = self.root / "Editor with spaces 'quoted' $literal" / "6000.3.27f2/Editor/Unity"
        self.editor.parent.mkdir(parents=True)
        shutil.copyfile(shutil.which("printf"), self.editor)
        self.editor.chmod(0o751)
        self.original = self.editor.read_bytes()
        self.original_inode = self.editor.stat().st_ino
        self.entry = self.root / "installed app with spaces 'quotes'" / "bin/unity-scale"
        self.entry.parent.mkdir(parents=True)
        self.entry.write_text("#!/usr/bin/python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n")
        self.entry.chmod(0o755)
        self.backup = self.editor.with_name("Unity.unity-scale-original")
        self.manifest = self.editor.with_name(".Unity.unity-scale.json")

    def tearDown(self):
        self.environment.stop()
        self.temporary.cleanup()

    def enable(self):
        return integration.enable(self.editor, entrypoint=self.entry)

    def run_editor(self, *args):
        return subprocess.run([str(self.editor), *args], capture_output=True, check=True)

    def test_enable_status_restore_exact_original_inode_bytes_mode(self):
        data=self.editor.parent/'Data'
        (data/'Managed').mkdir(parents=True)
        (data/'Managed/UnityEditor.dll').write_bytes(b'MZ fixture')
        (data/'Resources').mkdir()
        self.assertTrue(integration.status(self.editor)["can_enable"])
        self.assertTrue(self.enable()["enabled"])
        self.assertEqual(self.backup.read_bytes(), self.original)
        self.assertEqual(self.backup.stat().st_ino, self.original_inode)
        self.assertEqual(stat.S_IMODE(self.backup.stat().st_mode), 0o751)
        self.assertEqual(inspect_editor(self.editor).path, str(self.editor))
        self.assertEqual(len(integration.integration_records()), 1)
        self.assertEqual(integration.disable(self.editor)["state"], "disabled")
        self.assertEqual(self.editor.read_bytes(), self.original)
        self.assertEqual(self.editor.stat().st_ino, self.original_inode)
        self.assertEqual(stat.S_IMODE(self.editor.stat().st_mode), 0o751)
        self.assertEqual(integration.integration_records(), [])
        self.assertEqual(set(self.editor.parent.iterdir()), {self.editor, data})

    def test_forward_hub_argv_verbatim_and_keep_original_profile_identity(self):
        self.enable()
        args = ["-projectPath", "/path with 'quotes' and $dollar", "-logFile", "-", "", "line\nbreak", "--future-option"]
        self.assertEqual(json.loads(self.run_editor(*args).stdout), ["run", "--editor", str(self.editor), "--real-executable", str(self.backup), "--", *args])

    def test_headless_utility_and_license_invocations_bypass_app(self):
        self.enable()
        for flag in ("-batchmode", "-nographics", "-version", "--version", "-help", "--help", "-returnlicense", "-manualLicenseFile", "-createManualActivationFile"):
            with self.subTest(flag=flag):
                args = ["%s\\0", flag, "value with spaces", ""]
                self.assertEqual(self.run_editor(*args).stdout, b"\0".join(a.encode() for a in args[1:]) + b"\0")

    def test_app_removed_falls_back_without_package_import(self):
        self.enable()
        self.entry.unlink()
        self.assertEqual(self.run_editor("%s\\0", "hello space", "").stdout, b"hello space\0\0")

    def test_nonexecutable_app_falls_back(self):
        self.enable()
        self.entry.chmod(0o644)
        self.assertEqual(self.run_editor("%s", "direct").stdout, b"direct")

    def test_enable_disable_idempotent(self):
        self.enable()
        wrapper = self.editor.read_bytes()
        self.enable()
        self.assertEqual(self.editor.read_bytes(), wrapper)
        integration.disable(self.editor)
        self.assertEqual(integration.disable(self.editor)["state"], "disabled")

    def test_existing_backup_is_never_clobbered(self):
        self.backup.write_bytes(b"someone else's data")
        self.assertEqual(integration.status(self.editor)["state"], "conflict")
        with self.assertRaises(AppError):
            self.enable()
        self.assertEqual(self.backup.read_bytes(), b"someone else's data")
        self.assertEqual(self.editor.read_bytes(), self.original)

    def test_symlink_backup_is_never_followed(self):
        victim = self.root / "victim"
        victim.write_bytes(b"untouched")
        self.backup.symlink_to(victim)
        with self.assertRaises(AppError):
            self.enable()
        self.assertEqual(victim.read_bytes(), b"untouched")

    def test_update_replacing_wrapper_is_never_overwritten(self):
        self.enable()
        changed = b"\x7fELFupdated Editor installation"
        update = self.editor.with_name("new-Unity")
        update.write_bytes(changed)
        update.chmod(0o751)
        os.replace(update, self.editor)
        self.assertEqual(integration.status(self.editor)["state"], "conflict")
        with self.assertRaises(AppError):
            integration.disable(self.editor)
        self.assertEqual(self.editor.read_bytes(), changed)
        self.assertEqual(self.backup.read_bytes(), self.original)

    def test_modified_backup_prevents_restore(self):
        self.enable()
        self.backup.write_bytes(b"\x7fELFtampered")
        wrapper = self.editor.read_bytes()
        with self.assertRaises(AppError):
            integration.disable(self.editor)
        self.assertEqual(self.editor.read_bytes(), wrapper)
        self.assertEqual(integration.status(self.editor)["state"], "conflict")

    def test_changed_backup_mode_prevents_restore(self):
        self.enable()
        self.backup.chmod(0o755)
        with self.assertRaises(AppError):
            integration.disable(self.editor)

    def test_changed_wrapper_mode_prevents_restore(self):
        self.enable()
        self.editor.chmod(0o755)
        with self.assertRaises(AppError):
            integration.disable(self.editor)

    def test_invalid_or_foreign_record_is_never_removed(self):
        self.manifest.write_text('{"owner": "another app"}')
        with self.assertRaises(AppError):
            self.enable()
        with self.assertRaises(AppError):
            integration.disable(self.editor)
        self.assertTrue(self.manifest.exists())
        self.assertEqual(self.editor.read_bytes(), self.original)

    def test_non_native_launcher_rejected(self):
        self.editor.write_text("#!/bin/sh\nexit 0\n")
        with self.assertRaises(AppError):
            self.enable()
        self.assertFalse(self.backup.exists())

    def test_missing_app_aborts_before_any_editor_mutation(self):
        self.entry.unlink()
        with self.assertRaises(AppError):
            self.enable()
        self.assertEqual(list(self.editor.parent.iterdir()), [self.editor])

    def test_atomic_replacement_failure_rolls_back_staging_files(self):
        real_replace = os.replace
        def fail_editor_replace(source, target):
            if Path(target) == self.editor:
                raise OSError("simulated failed replacement")
            return real_replace(source, target)
        with patch.object(integration.os, "replace", side_effect=fail_editor_replace):
            with self.assertRaises(AppError):
                self.enable()
        self.assertEqual(list(self.editor.parent.iterdir()), [self.editor])
        self.assertEqual(self.editor.read_bytes(), self.original)
        self.assertEqual(self.editor.stat().st_ino, self.original_inode)
        self.assertEqual(integration.integration_records(), [])

    def test_interrupted_before_replace_can_be_cleaned_without_overwrite(self):
        self.enable()
        self.editor.unlink()
        os.link(self.backup, self.editor)
        self.assertTrue(integration.status(self.editor)["can_disable"])
        integration.disable(self.editor)
        self.assertEqual(self.editor.read_bytes(), self.original)
        self.assertEqual(self.editor.stat().st_ino, self.original_inode)
        self.assertFalse(self.manifest.exists())

    def test_interrupted_missing_launcher_can_restore_original(self):
        self.enable()
        self.editor.unlink()
        self.assertTrue(integration.status(self.editor)["can_disable"])
        integration.disable(self.editor)
        self.assertEqual(self.editor.read_bytes(), self.original)

    def test_interrupted_after_restore_can_remove_own_manifest(self):
        self.enable()
        os.replace(self.backup, self.editor)
        self.assertTrue(integration.status(self.editor)["can_disable"])
        integration.disable(self.editor)
        self.assertEqual(self.editor.read_bytes(), self.original)
        self.assertFalse(self.manifest.exists())

    def test_missing_backup_with_wrapper_cannot_be_disabled(self):
        self.enable()
        self.backup.unlink()
        self.assertFalse(integration.status(self.editor)["can_disable"])
        with self.assertRaises(AppError):
            integration.disable(self.editor)

    def test_direct_launch_resolves_only_verified_owned_backup(self):
        self.assertIsNone(integration.original_executable(self.editor))
        self.enable()
        self.assertEqual(integration.original_executable(self.editor), str(self.backup))
        self.backup.write_bytes(b"tampered")
        with self.assertRaises(AppError):
            integration.original_executable(self.editor)

    def test_app_failure_after_start_never_launches_editor_twice(self):
        self.enable()
        self.entry.write_text("#!/usr/bin/python3\nimport sys\nprint('app started')\nsys.exit(7)\n")
        result = subprocess.run([str(self.editor), "%s", "Editor started"], capture_output=True)
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout, b"app started\n")

    def test_graphical_credential_options_continue_through_scaling_app(self):
        self.enable()
        for flag in ("-serial", "-username", "-password", "-licenseType"):
            with self.subTest(flag=flag):
                args = ["-projectPath", "/example project", flag, "credential-value"]
                self.assertEqual(json.loads(self.run_editor(*args).stdout)[-len(args):], args)

    def test_explicit_changed_installation_requires_disable_then_enable(self):
        self.enable()
        other = self.root / "another-install/bin/unity-scale"
        other.parent.mkdir(parents=True)
        shutil.copy2(self.entry, other)
        before = self.editor.read_bytes()
        with self.assertRaisesRegex(AppError, "another installation of this app"):
            integration.enable(self.editor, entrypoint=other)
        self.assertEqual(self.editor.read_bytes(), before)

    def test_default_preview_preserves_existing_installation_binding(self):
        self.enable()
        # The implicit package entry point may differ when previewing source.
        with patch.object(integration, "ROOT", self.entry.parent.parent.parent):
            # Even a missing implicit preview entry must not rebind an already
            # enabled, verified installation.
            self.assertFalse((integration.ROOT / "bin/unity-scale").exists())
            self.assertTrue(integration.enable(self.editor)["enabled"])
        record = json.loads(self.manifest.read_text())
        self.assertEqual(record["entrypoint"], str(self.entry))

    def test_template_update_requires_explicit_disable_and_reenable(self):
        self.enable()
        existing_wrapper = integration._wrapper
        before = self.editor.read_bytes()
        with patch.object(integration, "_wrapper", side_effect=lambda *a: existing_wrapper(*a) + b"# newer template\n"):
            with self.assertRaisesRegex(AppError, "older scaling launcher"):
                self.enable()
        self.assertEqual(self.editor.read_bytes(), before)
        self.assertTrue(integration.status(self.editor)["can_disable"])

    def test_existing_scaled_launchers_bypass_nested_bridge(self):
        self.enable()
        for environment in ({"UNITY_SCALE_ACTIVE": "1"}, {"SCALING_AUDIT_VARIANT": "launcher-supersampling"}):
            with self.subTest(environment=environment), patch.dict(os.environ, environment):
                self.assertEqual(self.run_editor("%s", "already scaled").stdout, b"already scaled")

    def test_folder_input_supported(self):
        self.assertTrue(integration.enable(self.editor.parent.parent, entrypoint=self.entry)["enabled"])
        integration.disable(self.editor.parent)
        self.assertEqual(self.editor.read_bytes(), self.original)

    def test_wrapper_leaves_a_flatpak_sandbox_through_the_host_spawn(self):
        self.enable()
        content = self.editor.read_text()
        self.assertIn('flatpak-spawn", ["flatpak-spawn", "--host"', content)
        self.assertIn('"/.flatpak-info"', content)
        compile(content, str(self.editor), 'exec')


if __name__ == "__main__":
    unittest.main()
