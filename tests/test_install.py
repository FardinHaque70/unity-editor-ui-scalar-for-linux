import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
ROOT=Path(__file__).resolve().parent.parent
spec=importlib.util.spec_from_file_location('unity_scale_installer',ROOT/'packaging/install.py')
installer=importlib.util.module_from_spec(spec);spec.loader.exec_module(installer)


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
        self.prefix=self.root/'local with spaces'
        self.env=patch.dict(os.environ,{'XDG_CONFIG_HOME':str(self.root/'config'),'XDG_STATE_HOME':str(self.root/'state'),'XDG_DATA_HOME':str(self.root/'data')});self.env.start()
    def tearDown(self):self.env.stop();self.temp.cleanup()
    def test_user_install_entrypoint_runs_outside_checkout_and_uninstall_restores_files(self):
        destination=installer.install(self.prefix,skip_dependency_check=True)
        cmd=self.prefix/'bin/unity-scale'
        result=subprocess.run([str(cmd),'--version'],cwd='/tmp',capture_output=True,text=True)
        self.assertEqual(result.returncode,0,result.stderr);self.assertIn('0.1.0-beta.1',result.stdout)
        self.assertTrue((destination/'libexec/xwayland-satellite').exists())
        self.assertTrue((destination/'bridge/unity-scaling-xwayland-satellite-v5.2/Cargo.lock').exists())
        unrelated=self.prefix/'share/unrelated.txt';unrelated.write_text('keep')
        installer.uninstall(self.prefix)
        self.assertFalse(cmd.exists());self.assertFalse(destination.exists());self.assertEqual(unrelated.read_text(),'keep')
    def test_conflicting_command_is_not_overwritten_or_install_partially_created(self):
        command=self.prefix/'bin/unity-scale';command.parent.mkdir(parents=True);command.write_text('unrelated')
        with self.assertRaises(RuntimeError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertEqual(command.read_text(),'unrelated');self.assertFalse(installer.app_path(self.prefix).exists())
    def test_modified_installed_file_is_left_intact(self):
        installer.install(self.prefix,skip_dependency_check=True)
        cmd=self.prefix/'bin/unity-scale';cmd.write_text(cmd.read_text()+'# modified\n')
        with self.assertRaises(RuntimeError):installer.uninstall(self.prefix)
        self.assertTrue(cmd.exists());self.assertTrue(installer.app_path(self.prefix).exists())
    def test_upgrade_preserves_preferences_and_uninstall_does_not_delete_them(self):
        from unity_scale.core import Store
        Store().set_profile('/example/Editor/Unity',{'ui_scale_percent':160})
        installer.install(self.prefix,skip_dependency_check=True);installer.install(self.prefix,skip_dependency_check=True)
        installer.uninstall(self.prefix)
        self.assertEqual(Store().get_profile('/example/Editor/Unity')['ui_scale_percent'],160)

    def snapshot(self):
        destination=installer.app_path(self.prefix)
        files=[destination/'install-manifest.json',destination/'old-payload-sentinel',*installer._external_paths(self.prefix)]
        return {str(p):(p.read_bytes(),p.stat().st_mode) for p in files if p.is_file()}

    def test_full_upgrade_rollback_after_second_external_promotion_fails(self):
        destination=installer.install(self.prefix,skip_dependency_check=True)
        (destination/'old-payload-sentinel').write_text('previous version must survive')
        before=self.snapshot();real_replace=os.replace
        desktop=installer._external_paths(self.prefix)[1]
        failed=False
        def fail_once(source,target):
            nonlocal failed
            if Path(target)==desktop and Path(source).name.startswith('.unity-scale-stage-') and not failed:
                failed=True;raise OSError('injected full filesystem')
            return real_replace(source,target)
        with patch.object(installer.os,'replace',side_effect=fail_once):
            with self.assertRaises(OSError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertTrue(failed);self.assertEqual(self.snapshot(),before)
        self.assertFalse(list(destination.parent.glob('.unity-scale-previous-*')))
        self.assertFalse(list(self.prefix.rglob('.unity-scale-stage-*')))

    def test_fresh_install_failure_leaves_no_owned_payload_or_command(self):
        real_replace=os.replace;icon=installer._external_paths(self.prefix)[2]
        def fail_icon(source,target):
            if Path(target)==icon:raise OSError('injected promotion failure')
            return real_replace(source,target)
        with patch.object(installer.os,'replace',side_effect=fail_icon):
            with self.assertRaises(OSError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertFalse(installer.app_path(self.prefix).exists())
        for path in installer._external_paths(self.prefix):self.assertFalse(path.exists())

    def test_staging_failure_preserves_existing_payload_and_external_files(self):
        destination=installer.install(self.prefix,skip_dependency_check=True)
        (destination/'old-payload-sentinel').write_text('keep')
        before=self.snapshot()
        with patch.object(installer,'_temporary_file',side_effect=OSError('no space')):
            with self.assertRaises(OSError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertEqual(self.snapshot(),before)

    def test_upgrade_refuses_changed_icon_before_changing_payload(self):
        destination=installer.install(self.prefix,skip_dependency_check=True)
        (destination/'old-payload-sentinel').write_text('keep')
        installer._external_paths(self.prefix)[2].write_text('user-customized icon')
        before=self.snapshot()
        with self.assertRaises(RuntimeError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertEqual(self.snapshot(),before)

    def test_foreign_previous_directory_never_removed(self):
        previous=installer.app_path(self.prefix).with_name('.application-previous')
        previous.mkdir(parents=True)
        (previous/'install-manifest.json').write_text('{"marker":"someone else"}')
        with self.assertRaises(RuntimeError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertTrue(previous.exists())
        self.assertFalse(installer.app_path(self.prefix).exists())

    def test_malformed_manifest_owned_paths_cannot_delete_unrelated_file(self):
        destination=installer.install(self.prefix,skip_dependency_check=True)
        unrelated=self.prefix/'share/unrelated.txt';unrelated.write_text('keep')
        manifest_path=destination/'install-manifest.json';manifest=json.loads(manifest_path.read_text())
        manifest['files']={str(unrelated):hashlib.sha256(unrelated.read_bytes()).hexdigest()}
        manifest_path.write_text(json.dumps(manifest))
        with self.assertRaises(RuntimeError):installer.uninstall(self.prefix)
        self.assertEqual(unrelated.read_text(),'keep');self.assertTrue(destination.exists())

    def test_invalid_files_shape_is_actionable_without_mutation(self):
        destination=installer.install(self.prefix,skip_dependency_check=True)
        manifest_path=destination/'install-manifest.json';manifest=json.loads(manifest_path.read_text())
        manifest['files']=[];manifest_path.write_text(json.dumps(manifest))
        with self.assertRaises(RuntimeError):installer.uninstall(self.prefix)
        with self.assertRaises(RuntimeError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertTrue(destination.exists())

    def test_symlink_external_file_or_ancestor_is_refused(self):
        outside=self.root/'outside';outside.mkdir();victim=outside/'victim';victim.write_text('keep')
        command=self.prefix/'bin/unity-scale';command.parent.mkdir(parents=True);command.symlink_to(victim)
        with self.assertRaises(RuntimeError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertEqual(victim.read_text(),'keep');command.unlink();command.parent.rmdir()
        command.parent.symlink_to(outside,target_is_directory=True)
        with self.assertRaises(RuntimeError):installer.install(self.prefix,skip_dependency_check=True)
        self.assertTrue(command.parent.is_symlink())

    def test_uninstall_preflight_happens_before_editor_restore(self):
        from unity_scale import integration
        destination=installer.install(self.prefix,skip_dependency_check=True)
        installer._external_paths(self.prefix)[0].write_text('modified command')
        with patch.object(integration,'disable') as disable:
            with self.assertRaises(RuntimeError):installer.uninstall(self.prefix)
            disable.assert_not_called()
        self.assertTrue(destination.exists())

    def test_uninstall_restores_only_this_installations_integrations(self):
        from unity_scale import integration
        destination=installer.install(self.prefix,skip_dependency_check=True)
        records=[{'editor_path':'/owned/Editor/Unity','entrypoint':str(destination/'bin/unity-scale')},
                 {'editor_path':'/other/Editor/Unity','entrypoint':'/another/app/bin/unity-scale'}]
        with patch.object(integration,'integration_records',return_value=records), patch.object(integration,'status',return_value={'state':'enabled','can_disable':True}), patch.object(integration,'disable') as disable:
            installer.uninstall(self.prefix)
            disable.assert_called_once_with('/owned/Editor/Unity')

    def test_all_editor_integrations_preflight_before_any_restore(self):
        from unity_scale import integration
        destination=installer.install(self.prefix,skip_dependency_check=True)
        records=[{'editor_path':'/one/Unity','entrypoint':str(destination/'bin/unity-scale')},
                 {'editor_path':'/two/Unity','entrypoint':str(destination/'bin/unity-scale')}]
        statuses=[{'state':'enabled','can_disable':True},{'state':'conflict','can_disable':False,'detail':'changed backup'}]
        with patch.object(integration,'integration_records',return_value=records), patch.object(integration,'status',side_effect=statuses), patch.object(integration,'disable') as disable:
            with self.assertRaises(RuntimeError):installer.uninstall(self.prefix)
            disable.assert_not_called()
        self.assertTrue(destination.exists())

    def _fake_source_tree(self):
        root=self.root/'app';(root/'packaging').mkdir(parents=True)
        for name in ('install-deps.sh','build-bridge.sh'):(root/'packaging'/name).write_text('')
        return root

    def test_with_dependencies_installs_packages_and_builds_missing_bridge(self):
        root=self._fake_source_tree();calls=[]
        def fake(arguments,label):
            calls.append(arguments)
            if arguments[0].endswith('build-bridge.sh'):
                (root/'libexec').mkdir(exist_ok=True);(root/'libexec/xwayland-satellite').write_text('')
        with patch.object(installer,'ROOT',root), patch.object(installer,'_run_script',side_effect=fake):
            installer.ensure_dependencies()
        self.assertEqual(calls,[[str(root/'packaging/install-deps.sh')],[str(root/'packaging/build-bridge.sh')]])

    def test_with_dependencies_uses_runtime_only_for_a_present_bridge(self):
        root=self._fake_source_tree();(root/'libexec').mkdir();(root/'libexec/xwayland-satellite').write_text('')
        with patch.object(installer,'ROOT',root), patch.object(installer,'_run_script') as run:
            installer.ensure_dependencies()
        run.assert_called_once_with([str(root/'packaging/install-deps.sh'),'--runtime-only'],'Dependency installation')

    def test_with_dependencies_failure_stops_before_installing(self):
        with patch.object(installer,'ensure_dependencies',side_effect=RuntimeError('Dependency installation failed with exit status 1.')) as dependencies, patch.object(installer,'install') as install, patch.object(sys,'argv',['install.py','--with-dependencies','--prefix',str(self.prefix)]):
            self.assertEqual(installer.main(),1)
        dependencies.assert_called_once_with();install.assert_not_called()

    def test_without_dependencies_skips_package_installation(self):
        with patch.object(installer,'ensure_dependencies') as dependencies, patch.object(installer,'install',return_value=self.prefix/'app') as install, patch.object(sys,'argv',['install.py','--skip-dependency-check','--prefix',str(self.prefix)]):
            self.assertEqual(installer.main(),0)
        dependencies.assert_not_called();install.assert_called_once_with(self.prefix,skip_dependency_check=True)


if __name__=='__main__':unittest.main()
