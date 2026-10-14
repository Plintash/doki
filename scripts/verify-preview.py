#!/usr/bin/env python3
"""Validate the assembled and round-tripped preview without opening its UI."""
import json
import pathlib
import plistlib
import subprocess
import sys


def verify(app):
    app = pathlib.Path(app)
    contents = app / 'Contents'
    with (contents / 'Info.plist').open('rb') as f:
        info = plistlib.load(f)
    if info.get('CFBundleIdentifier') != 'sh.doki' or info.get('CFBundleExecutable') != 'Doki':
        raise ValueError('Unexpected app identity')
    if 'SUFeedURL' in info:
        raise ValueError('Preview must not enable an automatic update feed')
    for relative in [
        'MacOS/Doki', 'MacOS/waku-daemon', 'Resources/waku_js_repl',
        'Helpers/Doki Computer Use.app/Contents/MacOS/Doki Computer Use',
        'Helpers/Doki Computer Use.app/Contents/Frameworks/libcua_driver_sdk.dylib',
        'Frameworks/Sparkle.framework/Sparkle',
    ]:
        path = contents / relative
        if not path.is_file():
            raise ValueError(f'Missing runtime: {relative}')
        # Native binaries must contain arm64 and no machine-local dylib paths.
        subprocess.run(['lipo', '-verify_arch', 'arm64', str(path)], check=True)
        dependencies = subprocess.check_output(['otool', '-L', str(path)], text=True)
        for line in dependencies.splitlines()[1:]:
            dependency = line.strip().split(' (', 1)[0]
            if not dependency.startswith(('@', '/System/Library/', '/usr/lib/')):
                raise ValueError(f'Non-portable dependency: {dependency}')
    for relative in ['Resources/AppIcon.icns', 'Resources/computer-use/pi-extension.ts',
                     'Resources/skills/waku-computer-use/SKILL.md']:
        if not (contents / relative).is_file():
            raise ValueError(f'Missing resource: {relative}')
    for path in app.rglob('*'):
        if path.is_symlink() and not path.resolve().is_relative_to(app.resolve()):
            raise ValueError(f'Symlink escapes app: {path}')


def smoke_repl(app):
    executable = pathlib.Path(app) / 'Contents/Resources/waku_js_repl'
    requests = [
        dict(jsonrpc='2.0', id=1, method='initialize', params=dict(
            protocolVersion='2025-06-18', capabilities={},
            clientInfo=dict(name='doki-preview', version='1'))),
        dict(jsonrpc='2.0', id=2, method='tools/list', params={}),
    ]
    result = subprocess.run([str(executable)], input='\n'.join(map(json.dumps, requests)) + '\n',
                            capture_output=True, text=True, check=True, timeout=30)
    responses = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
    tools = next((response.get('result', {}).get('tools', [])
                  for response in responses if response.get('id') == 2), [])
    if [tool.get('name') for tool in tools] != ['js', 'js_reset']:
        raise ValueError('Bundled REPL failed its MCP tools/list smoke test')


if __name__ == '__main__':
    verify(sys.argv[1])
    if '--smoke' in sys.argv[2:]:
        smoke_repl(sys.argv[1])
