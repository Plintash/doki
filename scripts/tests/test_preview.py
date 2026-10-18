import importlib.util
import pathlib
import plistlib
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('preview', ROOT / 'scripts/verify-preview.py')
preview = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preview)


class PreviewTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.app = pathlib.Path(self.temp.name) / 'Doki.app'
        self.contents = self.app / 'Contents'
        self.contents.mkdir(parents=True)
        self.info = dict(CFBundleIdentifier='sh.doki', CFBundleExecutable='Doki')
        self.write_info()
        for path in [
            'MacOS/Doki', 'MacOS/waku-daemon', 'Resources/waku_js_repl',
            'Helpers/Doki Computer Use.app/Contents/MacOS/Doki Computer Use',
            'Helpers/Doki Computer Use.app/Contents/Frameworks/libcua_driver_sdk.dylib',
            'Frameworks/Sparkle.framework/Sparkle', 'Resources/AppIcon.icns',
            'Resources/computer-use/pi-extension.ts',
            'Resources/skills/waku-computer-use/SKILL.md',
        ]:
            file = self.contents / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.touch()
        self.addCleanup(patch.stopall)
        self.run = patch.object(preview.subprocess, 'run').start()
        self.otool = patch.object(preview.subprocess, 'check_output', return_value='binary:\n\t/usr/lib/libSystem.B.dylib (compatibility version 1)\n').start()

    def write_info(self):
        with (self.contents / 'Info.plist').open('wb') as f:
            plistlib.dump(self.info, f)

    def test_complete_bundle(self):
        preview.verify(self.app)
        self.assertEqual(self.run.call_count, 6)
        for invocation in self.run.call_args_list:
            command = invocation.args[0]
            self.assertEqual(command[0], 'lipo')
            self.assertTrue(pathlib.Path(command[1]).is_file())
            self.assertEqual(command[2:], ['-verify_arch', 'arm64'])
            self.assertEqual(invocation.kwargs, dict(check=True))

    def test_dependencies_inspect_arm64_slice_only(self):
        preview.verify(self.app)
        self.assertEqual(self.otool.call_count, 6)
        for invocation in self.otool.call_args_list:
            command = invocation.args[0]
            self.assertEqual(command[:4], ['otool', '-arch', 'arm64', '-L'])
            self.assertEqual(len(command), 5)
            self.assertTrue(pathlib.Path(command[4]).is_file())
            self.assertEqual(invocation.kwargs, dict(text=True))

    def test_wrong_architecture_rejected(self):
        self.run.side_effect = subprocess.CalledProcessError(1, 'lipo')
        with self.assertRaises(subprocess.CalledProcessError):
            preview.verify(self.app)
        self.otool.assert_not_called()

    def test_no_empty_release_env(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        self.assertNotRegex(workflow, r'(?m)^ +env:\s*\n +run:')

    def test_missing_daemon(self):
        (self.contents / 'MacOS/waku-daemon').unlink()
        with self.assertRaisesRegex(ValueError, 'Missing runtime'):
            preview.verify(self.app)

    def test_upstream_feed_rejected(self):
        self.info['SUFeedURL'] = 'https://example.invalid/appcast.xml'
        self.write_info()
        with self.assertRaisesRegex(ValueError, 'update feed'):
            preview.verify(self.app)

    def test_external_library_rejected(self):
        self.otool.return_value = 'binary:\n\t/opt/homebrew/lib/libfoo.dylib (compatibility version 1)\n'
        with self.assertRaisesRegex(ValueError, 'Non-portable'):
            preview.verify(self.app)

    def test_escaping_symlink_rejected(self):
        (self.contents / 'escape').symlink_to(self.temp.name)
        with self.assertRaisesRegex(ValueError, 'Symlink escapes'):
            preview.verify(self.app)

    def test_wrong_identity_rejected(self):
        self.info['CFBundleIdentifier'] = 'sh.waku'
        self.write_info()
        with self.assertRaisesRegex(ValueError, 'identity'):
            preview.verify(self.app)

    def test_repl_smoke_contract(self):
        self.run.return_value.stdout = '{"id":2,"result":{"tools":[{"name":"js"},{"name":"js_reset"}]}}\n'
        preview.smoke_repl(self.app)
        self.assertEqual(self.run.call_args.kwargs['timeout'], 30)

    def test_repl_wrong_tools_rejected(self):
        self.run.return_value.stdout = '{"id":2,"result":{"tools":[]}}\n'
        with self.assertRaisesRegex(ValueError, 'MCP tools/list'):
            preview.smoke_repl(self.app)

    def test_no_privileged_pr_trigger(self):
        workflow = (ROOT / '.github/workflows/preview.yml').read_text()
        self.assertNotIn('pull_request_target', workflow)
        self.assertNotIn('$' + '{{ secrets.', workflow)
        self.assertIn('persist-credentials: false', workflow)
        self.assertIn('--prerelease --latest=false', workflow)


if __name__ == '__main__':
    unittest.main()

