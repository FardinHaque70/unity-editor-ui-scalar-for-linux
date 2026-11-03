#!/usr/bin/env python3
"""User-local install/uninstall with an owned-file manifest. No network or sudo unless --with-dependencies is requested."""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
APP_ID = 'io.github.unityscale.UnityScale'
APP_NAME = 'Unity Editor UI Scalar for Linux'
MARKER = 'UNITY_SCALE_INSTALL_V1'
PAYLOAD_DIRS = ('unity_scale', 'bin', 'libexec', 'resources', 'bridge', 'docs', 'packaging')
PAYLOAD_FILES = ('README.md', 'LICENSE', 'NOTICE', 'CHANGELOG.md', 'pyproject.toml', 'install.sh', 'uninstall.sh')


def prefix_root(value=None):
    return Path(value).expanduser().resolve() if value else Path.home()/'.local'


def app_path(prefix):
    return prefix/'share/unity-scale/application'


def _desktop_quote(value):
    return '"'+str(value).replace('\\','\\\\').replace('"','\\"').replace('`','\\`').replace('$','\\$').replace('%','%%')+'"'


def _write(path, text, executable=False):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(text)
    if executable:path.chmod(0o755)


def check_dependencies():
    problems=[]
    try:
        import gi
        gi.require_version('Gtk','4.0');gi.require_version('Adw','1')
        from gi.repository import Gtk, Adw
        if (Gtk.get_major_version(),Gtk.get_minor_version()) < (4,10):problems.append('GTK4.10 or newer')
        if (Adw.get_major_version(),Adw.get_minor_version()) < (1,4):problems.append('libadwaita1.4 or newer')
    except (ImportError,ValueError) as exc:problems.append('Python GObject, GTK4 and libadwaita: '+str(exc))
    for binary in ['Xwayland','kscreen-doctor']:
        if not shutil.which(binary):problems.append(binary)
    if problems:raise RuntimeError('Missing dependencies: '+', '.join(problems)+'. See README.md for distribution package commands.')
    helper=ROOT/'libexec/xwayland-satellite'
    if not helper.is_file() or not os.access(helper,os.X_OK):raise RuntimeError('Build the scaling bridge first: ./packaging/build-bridge.sh')
    result=subprocess.run(['ldd',str(helper)],capture_output=True,text=True,timeout=10)
    if result.returncode or 'not found' in result.stdout+result.stderr:
        raise RuntimeError('The bundled bridge cannot run on this system. Build it locally using ./packaging/build-bridge.sh.\n'+result.stdout+result.stderr)


def _external_paths(prefix):
    return (prefix/'bin/unity-scale',
            prefix/'share/applications'/f'{APP_ID}.desktop',
            prefix/'share/icons/hicolor/scalable/apps'/f'{APP_ID}.svg')


def _no_symlinks(path, prefix):
    """Reject symlinked owned paths and ancestor directories below the prefix."""
    try:relative=path.relative_to(prefix)
    except ValueError:raise RuntimeError('Unsafe owned-file path outside the installation prefix.')
    current=prefix
    for component in relative.parts:
        current=current/component
        if current.is_symlink():raise RuntimeError(f'Refusing a symlinked installation path: {current}')


def _read_manifest(destination, prefix):
    _no_symlinks(destination, prefix)
    manifest_path=destination/'install-manifest.json'
    if not destination.is_dir() or not manifest_path.is_file() or manifest_path.is_symlink():
        raise RuntimeError(f'Unrecognized installation directory: {destination}')
    if manifest_path.stat().st_size > 1024*1024:raise RuntimeError('Installation manifest is too large.')
    try:manifest=json.loads(manifest_path.read_text())
    except (ValueError,OSError) as exc:raise RuntimeError('Cannot read the installation manifest; nothing was removed.') from exc
    expected={str(path) for path in _external_paths(prefix)}
    if (not isinstance(manifest,dict) or manifest.get('marker')!=MARKER
            or manifest.get('prefix')!=str(prefix)
            or not isinstance(manifest.get('files'),dict)
            or set(manifest['files'])!=expected
            or any(not isinstance(digest,str) or len(digest)!=64
                   or any(c not in '0123456789abcdef' for c in digest)
                   for digest in manifest['files'].values())):
        raise RuntimeError('Invalid installation identity or owned-file manifest; nothing was removed.')
    return manifest


def _preflight_external(prefix, manifest):
    for path in _external_paths(prefix):
        _no_symlinks(path,prefix)
        if os.path.lexists(path):
            if not path.is_file():raise RuntimeError(f'Expected a regular owned file: {path}')
            if manifest is None:raise RuntimeError(f'Refusing to replace an existing unrelated file: {path}')
            if hashlib.sha256(path.read_bytes()).hexdigest()!=manifest['files'][str(path)]:
                raise RuntimeError(f'Installed file was changed; left untouched: {path}')


def _temporary_file(path, content, mode):
    path.parent.mkdir(parents=True,exist_ok=True)
    fd,name=tempfile.mkstemp(prefix='.unity-scale-stage-',dir=path.parent)
    try:
        with os.fdopen(fd,'wb') as stream:
            stream.write(content);stream.flush();os.fchmod(stream.fileno(),mode);os.fsync(stream.fileno())
        return Path(name)
    except BaseException:
        Path(name).unlink(missing_ok=True)
        raise


def _unused_backup(path):
    fd,name=tempfile.mkstemp(prefix='.unity-scale-previous-',dir=path.parent)
    os.close(fd);Path(name).unlink()
    return Path(name)


def _run_script(arguments, label):
    try:
        subprocess.run(arguments,check=True)
    except subprocess.CalledProcessError as exc:
        raise RuntimeError(f'{label} failed with exit status {exc.returncode}.') from exc
    except OSError as exc:
        raise RuntimeError(f'Could not run {arguments[0]}: {exc}') from exc


def ensure_dependencies():
    """Install distribution packages and build the bridge when it is missing."""
    helper=ROOT/'libexec/xwayland-satellite'
    dependency_script=[str(ROOT/'packaging/install-deps.sh')]
    if not Path(dependency_script[0]).is_file():
        raise RuntimeError('Missing packaging/install-deps.sh; install dependencies manually.')
    if helper.is_file():dependency_script.append('--runtime-only')
    _run_script(dependency_script,'Dependency installation')
    if not helper.is_file():
        _run_script([str(ROOT/'packaging/build-bridge.sh')],'Bridge build')
    if not helper.is_file():
        raise RuntimeError('The bridge build did not produce libexec/xwayland-satellite.')


def install(prefix, *, skip_dependency_check=False):
    prefix=prefix_root(prefix)
    if not skip_dependency_check:check_dependencies()
    destination=app_path(prefix)
    _no_symlinks(destination,prefix)
    manifest=_read_manifest(destination,prefix) if os.path.lexists(destination) else None
    previous=destination.with_name('.application-previous')
    if os.path.lexists(previous):
        raise RuntimeError(f'A previous-install recovery directory already exists; left untouched: {previous}')
    _preflight_external(prefix,manifest)
    destination.parent.mkdir(parents=True,exist_ok=True)
    stage=Path(tempfile.mkdtemp(prefix='.install-',dir=destination.parent))
    external_stages={};external_backups={};promoted=[]
    old=None;payload_promoted=False;committed=False
    try:
        for name in PAYLOAD_DIRS:
            source=ROOT/name
            if source.exists():shutil.copytree(source,stage/name,ignore=shutil.ignore_patterns('__pycache__','*.pyc','target','.git'))
        for name in PAYLOAD_FILES:
            if (ROOT/name).is_file():shutil.copy2(ROOT/name,stage/name)
        launcher,desktop,icon=_external_paths(prefix)
        launcher_text='#!/usr/bin/python3\n# '+MARKER+'\nimport os,sys\nos.execv("/usr/bin/python3",["/usr/bin/python3",'+repr(str(destination/'bin/unity-scale'))+',*sys.argv[1:]])\n'
        desktop_text=(stage/'resources'/f'{APP_ID}.desktop').read_text()
        lines=[]
        for line in desktop_text.splitlines():
            if line.startswith('Exec='):line='Exec='+_desktop_quote(launcher)
            elif line.startswith('TryExec='):line='TryExec='+str(launcher)
            elif line.startswith('Icon='):line='Icon='+str(icon)
            lines.append(line)
        if not any(x.startswith('X-UnityScale-Managed=') for x in lines):lines.append('X-UnityScale-Managed=true')
        contents={launcher:(launcher_text.encode(),0o755),desktop:(('\n'.join(lines)+'\n').encode(),0o644),
                  icon:((stage/'resources'/f'{APP_ID}.svg').read_bytes(),0o644)}
        final_manifest={'marker':MARKER,'prefix':str(prefix),
                        'files':{str(path):hashlib.sha256(content).hexdigest() for path,(content,mode) in contents.items()}}
        _write(stage/'install-manifest.json',json.dumps(final_manifest,indent=2)+'\n')
        for path,(content,mode) in contents.items():external_stages[path]=_temporary_file(path,content,mode)
        # All reads, copies and content generation have succeeded. Recheck owner
        # hashes immediately before the short sequence of atomic replacements.
        _preflight_external(prefix,manifest)
        if destination.exists():
            old=_unused_backup(destination)
            os.replace(destination,old)
        os.replace(stage,destination);payload_promoted=True
        for path,temporary in external_stages.items():
            if path.exists():
                backup=_unused_backup(path);os.replace(path,backup);external_backups[path]=backup
            os.replace(temporary,path);promoted.append(path)
        committed=True
    except BaseException as failure:
        rollback_errors=[]
        for path in reversed(tuple(external_stages)):
            try:
                if path in promoted:path.unlink(missing_ok=True)
                if path in external_backups:os.replace(external_backups[path],path)
            except OSError as exc:rollback_errors.append(str(exc))
        try:
            if payload_promoted:shutil.rmtree(destination)
            if old is not None and old.exists():os.replace(old,destination)
        except OSError as exc:rollback_errors.append(str(exc))
        if rollback_errors:
            raise RuntimeError('Installation failed; recovery files were retained. '+str(failure)+'; '+ '; '.join(rollback_errors)) from failure
        raise
    finally:
        if stage.exists():shutil.rmtree(stage)
        for path in external_stages.values():path.unlink(missing_ok=True)
        # Do not discard a backup unless every replacement committed, or it was
        # already moved back by rollback. This preserves recovery on I/O errors.
        if committed:
            for path in external_backups.values():path.unlink(missing_ok=True)
            if old is not None and old.exists():shutil.rmtree(old)
    if shutil.which('update-desktop-database'):
        try:subprocess.run(['update-desktop-database',str(desktop.parent)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        except OSError:pass
    if prefix == Path.home()/'.local' and shutil.which('kbuildsycoca6'):
        try:subprocess.run(['kbuildsycoca6','--noincremental'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=15)
        except (OSError,subprocess.SubprocessError):pass
    print(f'Installed {APP_NAME}. Open it from your application menu.\nCommand: {launcher}\nUninstall: {destination}/uninstall.sh')
    return destination


def uninstall(prefix):
    prefix=prefix_root(prefix)
    destination=app_path(prefix)
    manifest=_read_manifest(destination,prefix)
    # Complete ownership validation before changing any Editor or app file.
    _preflight_external(prefix,manifest)
    sys.path.insert(0,str(destination))
    from unity_scale import integration
    records=[record for record in integration.integration_records()
             if record.get('entrypoint')==str(destination/'bin/unity-scale')]
    failures=[]
    # Verify all selected integrations first; one conflict must not disable the
    # other Editors and then abort the requested uninstall halfway through.
    for record in records:
        state=integration.status(record['editor_path'])
        if state['state']!='disabled' and not state.get('can_disable'):
            failures.append(state['detail'])
    if failures:raise RuntimeError('Restore these Editor integrations before uninstalling:\n'+'\n'.join(failures))
    for record in records:
        try:integration.disable(record['editor_path'])
        except Exception as exc:failures.append(str(exc))
    if failures:raise RuntimeError('Restore these Editor integrations before uninstalling:\n'+'\n'.join(failures))
    _preflight_external(prefix,manifest)
    for path in _external_paths(prefix):
        if path.is_file():path.unlink()
    shutil.rmtree(destination)
    print(f'{APP_NAME} uninstalled. Editor executables restored. Preferences and logs retained in your XDG directories.')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--prefix',help='Install under this prefix (default ~/.local)')
    parser.add_argument('--uninstall',action='store_true')
    parser.add_argument('--with-dependencies',action='store_true',
                        help='Install distribution packages and build the bridge first (uses sudo)')
    parser.add_argument('--skip-dependency-check',action='store_true',help=argparse.SUPPRESS)
    args=parser.parse_args()
    try:
        prefix=prefix_root(args.prefix)
        if args.uninstall:uninstall(prefix)
        else:
            if args.with_dependencies:ensure_dependencies()
            install(prefix,skip_dependency_check=args.skip_dependency_check)
        return 0
    except (RuntimeError,OSError,ValueError) as exc:
        print(APP_NAME+' installer: '+str(exc),file=sys.stderr);return 1

if __name__=='__main__':raise SystemExit(main())
