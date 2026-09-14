"""Test staged uninstall and ownership checks without sudo or live desktop edits."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1]


class SuiteScripts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='cosmic-suite-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'suite with spaces'
        self.root.mkdir()
        self.stage = Path(self.temp.name) / 'staged root'
        self.comp = self.root / 'cosmic-comp-scrolling-prototype'
        self.comp.mkdir()
        for name in ('install.sh', 'uninstall.sh'):
            shutil.copy2(SOURCE / name, self.root / name)
        for name in ('install-scrolling-session.sh', 'start-scrolling-session.sh',
                     'cosmic-scrolling-test.desktop'):
            shutil.copy2(SOURCE / self.comp.name / name, self.comp / name)
        binary = self.comp / 'target/debug/cosmic-comp'
        binary.parent.mkdir(parents=True)
        binary.write_text('#!/bin/sh\nexit 0\n')
        binary.chmod(0o755)
        self.config = self.comp / 'target/scrolling-test-config'
        self.config.mkdir()
        (self.config / 'keep').write_text('settings')
        self.state = self.root / '.cosmic-scrolling'
        self.prefix = self.state / 'prefix'
        (self.prefix / 'bin').mkdir(parents=True)
        (self.prefix / 'bin/cosmic-applet-tiling').write_text('private binary')
        (self.prefix / 'bin/cosmic-comp').symlink_to(binary)
        (self.state / 'manifest').write_text('owner=cosmic-scrolling-prototype-v1\n')
        (self.state / 'cache').write_text('retained build')
        self.env = dict(os.environ)
        self.env.pop('DESTDIR', None)
        self.env.pop('XDG_CONFIG_HOME', None)
        mock = self.root / 'mock-bin'
        mock.mkdir()
        sudo = mock / 'sudo'
        sudo.write_text('#!/bin/sh\necho UNEXPECTED_SUDO >&2\nexit 99\n')
        sudo.chmod(0o755)
        self.env['PATH'] = str(mock) + ':' + os.defpath
        self.run_script(self.comp / 'install-scrolling-session.sh', '--destdir', self.stage)
        self.desktop = self.stage / 'usr/share/wayland-sessions/cosmic-scrolling-test.desktop'
        self.launcher = self.stage / 'usr/local/bin/cosmic-scrolling-test-session'
        self.normal = self.desktop.parent / 'cosmic.desktop'
        self.normal.write_text('distribution session')

    def run_script(self, script, *args, code=0, env=None):
        p = subprocess.run([str(script), *map(str, args)], env=env or self.env,
                           text=True, capture_output=True)
        self.assertEqual(p.returncode, code, p.stdout + p.stderr)
        return p

    def uninstall(self, *args, **kwargs):
        return self.run_script(self.root / 'uninstall.sh', '--destdir', self.stage,
                               *args, **kwargs)

    def assert_untouched(self):
        self.assertTrue(self.desktop.exists())
        self.assertTrue(self.launcher.is_symlink())
        self.assertTrue((self.state / 'manifest').exists())
        self.assertTrue(self.config.exists())

    def test_uninstall_is_idempotent_and_preserves_normal_session_cache_and_config(self):
        self.uninstall()
        self.uninstall()
        self.assertFalse(self.launcher.is_symlink())
        self.assertFalse(self.desktop.exists())
        self.assertFalse((self.state / 'manifest').exists())
        self.assertEqual(self.normal.read_text(), 'distribution session')
        self.assertEqual((self.state / 'cache').read_text(), 'retained build')
        self.assertEqual((self.config / 'keep').read_text(), 'settings')

    def test_purge_removes_only_isolated_config(self):
        self.uninstall('--purge-config')
        self.assertFalse(self.config.exists())
        self.assertTrue((self.comp / 'target/debug/cosmic-comp').exists())
        self.assertTrue(self.normal.exists())

    def test_foreign_launcher_prevents_all_removals(self):
        self.launcher.unlink()
        self.launcher.symlink_to('/another/clone/start-scrolling-session.sh')
        self.uninstall(code=1)
        self.assert_untouched()

    def test_foreign_desktop_prevents_all_removals(self):
        self.desktop.write_text('foreign')
        self.uninstall(code=1)
        self.assert_untouched()
        self.assertEqual(self.desktop.read_text(), 'foreign')

    def test_desktop_symlink_is_refused(self):
        target = self.root / 'owned-looking.desktop'
        self.desktop.rename(target)
        self.desktop.symlink_to(target)
        self.uninstall(code=1)
        self.assert_untouched()
        self.assertTrue(target.exists())

    def test_redirected_private_prefix_is_refused(self):
        elsewhere = self.root / 'unrelated'
        self.prefix.rename(elsewhere)
        self.prefix.symlink_to(elsewhere)
        self.uninstall(code=1)
        self.assert_untouched()
        self.assertTrue((elsewhere / 'bin/cosmic-applet-tiling').exists())

    def test_active_session_is_refused_before_removal(self):
        self.uninstall('--purge-config', code=1,
                       env=dict(self.env, XDG_CONFIG_HOME=str(self.config)))
        self.assert_untouched()

    def test_destdir_environment(self):
        self.run_script(self.root / 'uninstall.sh', env=dict(self.env, DESTDIR=str(self.stage)))
        self.assertFalse(self.desktop.exists())

    def test_invalid_arguments_do_not_change_installation(self):
        for script in ('install.sh', 'uninstall.sh'):
            for args in (('--unknown',), ('--destdir',), ('--destdir', 'relative')):
                self.run_script(self.root / script, *args, code=1)
                self.assert_untouched()


if __name__ == '__main__':
    unittest.main()
