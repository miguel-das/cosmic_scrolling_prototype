#!/usr/bin/env python3
"""Exercise real session scripts with a staged install and fake session processes.

Run: python3 -m unittest discover -s tests -p 'test_*.py' -v
No sudo, greeter installation, live configuration, or systemd manager is used.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1]


class SessionScripts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='cosmic-session-tests-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.clone = self.root / 'clone with spaces'
        self.clone.mkdir()
        for name in ('install-scrolling-session.sh', 'start-scrolling-session.sh',
                     'cosmic-scrolling-test.desktop'):
            shutil.copy2(SOURCE / name, self.clone / name)
        self.executable(self.clone / 'target/debug/cosmic-comp', '#!/bin/sh\nexit 0\n')
        self.stage = self.root / 'staged root'
        self.bin = self.root / 'mock-bin'
        self.bin.mkdir()
        self.env = os.environ.copy()
        for name in ('DESTDIR', 'DISPLAY', 'WAYLAND_DISPLAY', 'COSMIC_SCROLLING_TILING',
                     'COSMIC_SCROLLING_SESSION', 'XDG_CONFIG_HOME', 'RUST_LOG'):
            self.env.pop(name, None)
        self.env['PATH'] = str(self.bin) + os.pathsep + os.defpath
        self.env['SESSION_TEST_LOG'] = str(self.root / 'calls.jsonl')
        self.executable(self.bin / 'sudo', '#!/bin/sh\necho "UNEXPECTED SUDO" >&2\nexit 99\n')
        self.executable(self.bin / 'start-cosmic', '''#!/usr/bin/python3
import json, os, sys
with open(os.environ['SESSION_TEST_LOG'], 'a') as f:
    f.write(json.dumps({'session': sys.argv[1:], 'env': dict(os.environ)}) + '\\n')
sys.exit(int(os.environ.get('SESSION_TEST_EXIT', '0')))
''')
        self.executable(self.bin / 'systemctl', '''#!/usr/bin/python3
import json, os, sys
with open(os.environ['SESSION_TEST_LOG'], 'a') as f:
    f.write(json.dumps({'systemctl': sys.argv[1:]}) + '\\n')
''')

    def executable(self, path, text):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        path.chmod(0o755)

    def run_script(self, name, *args, code=0, env=None):
        result = subprocess.run([str(self.clone / name), *map(str, args)],
                                env=env or self.env, cwd=self.root,
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        return result

    def install(self, **kwargs):
        return self.run_script('install-scrolling-session.sh', '--destdir', self.stage, **kwargs)

    def calls(self):
        return [json.loads(line) for line in (self.root / 'calls.jsonl').read_text().splitlines()]

    @property
    def launcher(self):
        return self.stage / 'usr/local/bin/cosmic-scrolling-test-session'

    @property
    def desktop(self):
        return self.stage / 'usr/share/wayland-sessions/cosmic-scrolling-test.desktop'

    def test_install_is_standalone_idempotent_and_preserves_normal_session(self):
        normal = self.stage / 'usr/share/wayland-sessions/cosmic.desktop'
        normal.parent.mkdir(parents=True)
        normal.write_text('normal session\n')
        self.install()
        self.install()
        self.assertEqual(self.launcher.readlink(), self.clone / 'start-scrolling-session.sh')
        self.assertEqual(self.desktop.read_bytes(), (SOURCE / self.desktop.name).read_bytes())
        self.assertEqual(self.desktop.stat().st_mode & 0o777, 0o644)
        self.assertEqual(normal.read_text(), 'normal session\n')

    def test_reinstall_after_clone_moves(self):
        self.install()
        moved = self.root / 'moved clone'
        self.clone.rename(moved)
        self.clone = moved
        self.install()
        self.assertEqual(self.launcher.resolve(), self.clone / 'start-scrolling-session.sh')

    def test_destdir_environment(self):
        env = dict(self.env, DESTDIR=str(self.stage))
        self.run_script('install-scrolling-session.sh', env=env)
        self.assertTrue(self.launcher.is_symlink())

    def test_missing_build_changes_nothing(self):
        (self.clone / 'target/debug/cosmic-comp').unlink()
        self.install(code=1)
        self.assertFalse(self.stage.exists())

    def test_missing_desktop_template_changes_nothing(self):
        (self.clone / 'cosmic-scrolling-test.desktop').unlink()
        self.install(code=1)
        self.assertFalse(self.stage.exists())

    def test_invalid_arguments_change_nothing(self):
        for args in (('--unknown',), ('--destdir',), ('--destdir', 'relative')):
            self.run_script('install-scrolling-session.sh', *args, code=2)
        self.assertFalse(self.stage.exists())

    def test_foreign_desktop_is_preserved(self):
        self.desktop.parent.mkdir(parents=True)
        self.desktop.write_text('not owned\n')
        self.install(code=1)
        self.assertEqual(self.desktop.read_text(), 'not owned\n')
        self.assertFalse(self.launcher.exists())

    def test_foreign_launcher_file_is_preserved(self):
        self.launcher.parent.mkdir(parents=True)
        self.launcher.write_text('not ours\n')
        self.install(code=1)
        self.assertEqual(self.launcher.read_text(), 'not ours\n')
        self.assertFalse(self.desktop.exists())

    def test_foreign_launcher_symlink_is_preserved(self):
        self.launcher.parent.mkdir(parents=True)
        self.launcher.symlink_to('/some/other/launcher')
        self.install(code=1)
        self.assertEqual(str(self.launcher.readlink()), '/some/other/launcher')

    def test_desktop_symlink_does_not_overwrite_target(self):
        self.desktop.parent.mkdir(parents=True)
        target = self.root / 'foreign.desktop'
        target.write_bytes((SOURCE / 'cosmic-scrolling-test.desktop').read_bytes())
        self.desktop.symlink_to(target)
        before = target.read_bytes()
        self.install(code=1)
        self.assertEqual(target.read_bytes(), before)
        self.assertTrue(self.desktop.is_symlink())

    def test_launcher_uses_clone_and_system_session_without_private_applet(self):
        self.install()
        result = subprocess.run([str(self.launcher)], env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        session = self.calls()[0]
        env = session['env']
        self.assertEqual(session['session'], ['--in-login-shell'])
        self.assertEqual(env['PATH'].split(os.pathsep)[0], str(self.clone / 'target/debug'))
        self.assertEqual(env['XDG_CONFIG_HOME'], str(self.clone / 'target/scrolling-test-config'))
        self.assertEqual(env['COSMIC_SCROLLING_TILING'], '1')
        self.assertEqual(env['COSMIC_SCROLLING_SESSION'], '1')
        config = Path(env['XDG_CONFIG_HOME']) / 'cosmic/com.system76.CosmicComp/v1'
        self.assertEqual((config / 'autotile').read_text(), 'true\n')
        self.assertEqual((config / 'tiling_engine').read_text(), 'Scrolling\n')
        cleanup = [c['systemctl'] for c in self.calls()[1:]]
        self.assertIn(['--user', 'set-environment', 'PATH=' + self.env['PATH']], cleanup)
        for name in ('XDG_CONFIG_HOME', 'RUST_LOG', 'COSMIC_SCROLLING_TILING', 'COSMIC_SCROLLING_SESSION'):
            self.assertIn(['--user', 'unset-environment', name], cleanup)

    def test_launcher_preserves_config_and_restores_set_values_after_failure(self):
        config = self.clone / 'target/scrolling-test-config/cosmic/com.system76.CosmicComp/v1'
        config.mkdir(parents=True)
        (config / 'autotile').write_text('false\n')
        (config / 'tiling_engine').write_text('Classic\n')
        old = dict(XDG_CONFIG_HOME=str(self.root / 'normal config'), RUST_LOG='',
                   COSMIC_SCROLLING_TILING='no', COSMIC_SCROLLING_SESSION='previous')
        env = dict(self.env, **old, SESSION_TEST_EXIT='7')
        self.run_script('start-scrolling-session.sh', env=env, code=7)
        self.assertEqual((config / 'autotile').read_text(), 'false\n')
        self.assertEqual((config / 'tiling_engine').read_text(), 'Classic\n')
        self.assertFalse((self.root / 'normal config').exists())
        cleanup = [c['systemctl'] for c in self.calls()[1:]]
        for name, value in old.items():
            self.assertIn(['--user', 'set-environment', name + '=' + value], cleanup)

    def test_launcher_refuses_full_session_inside_running_desktop(self):
        for name in ('DISPLAY', 'WAYLAND_DISPLAY'):
            self.run_script('start-scrolling-session.sh', env=dict(self.env, **{name: 'test'}), code=1)
        self.assertFalse((self.root / 'calls.jsonl').exists())
        self.assertFalse((self.clone / 'target/scrolling-test-config').exists())

    def private_applet(self):
        state = self.clone.parent / '.cosmic-scrolling'
        self.executable(state / 'prefix/bin/cosmic-applet-tiling', '#!/bin/sh\nexit 0\n')
        desktop = state / 'prefix/share/applications/com.system76.CosmicAppletTiling.desktop'
        desktop.parent.mkdir(parents=True)
        desktop.write_text('[Desktop Entry]\n')
        (state / 'manifest').write_text('owner=cosmic-scrolling-prototype-v1\n')
        return state

    def test_private_applet_paths_and_environment_cleanup(self):
        state = self.private_applet()
        env = dict(self.env, XDG_DATA_DIRS='/custom/share:/usr/share')
        self.run_script('start-scrolling-session.sh', env=env)
        session = self.calls()[0]['env']
        self.assertEqual(session['PATH'].split(':')[0], str(state / 'prefix/bin'))
        self.assertEqual(session['XDG_DATA_DIRS'], str(state / 'prefix/share') + ':' + env['XDG_DATA_DIRS'])
        self.assertIn({'systemctl': ['--user', 'set-environment', 'XDG_DATA_DIRS=' + env['XDG_DATA_DIRS']]}, self.calls())

    def test_private_applet_default_data_path_is_unset_after_logout(self):
        state = self.private_applet()
        env = dict(self.env)
        env.pop('XDG_DATA_DIRS', None)
        self.run_script('start-scrolling-session.sh', env=env)
        self.assertEqual(self.calls()[0]['env']['XDG_DATA_DIRS'], str(state / 'prefix/share') + ':/usr/local/share:/usr/share')
        self.assertIn({'systemctl': ['--user', 'unset-environment', 'XDG_DATA_DIRS']}, self.calls())

    def test_incomplete_private_applet_refuses_silent_system_fallback(self):
        state = self.private_applet()
        (state / 'prefix/bin/cosmic-applet-tiling').unlink()
        self.run_script('start-scrolling-session.sh', code=1)
        self.assertFalse((self.root / 'calls.jsonl').exists())

    def test_launcher_missing_compositor_changes_nothing(self):
        (self.clone / 'target/debug/cosmic-comp').unlink()
        self.run_script('start-scrolling-session.sh', code=1)
        self.assertFalse((self.root / 'calls.jsonl').exists())


if __name__ == '__main__':
    unittest.main()
