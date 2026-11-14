#!/usr/bin/env python3
"""Actual signed Apple sandbox, owned temporary files, no permission bypass.

The Rust probe owns and removes its fixtures. No user document is opened.
This development signature does not qualify production signing or notarization.
"""
import os
import argparse
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--bundled-source', action='store_true', help='Read an owned source created outside the sandbox before signing')
args = parser.parse_args()
if platform.system() != 'Darwin':
    raise SystemExit('Apple publication metadata qualification requires macOS')
rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
env = os.environ.copy()
env['PATH'] = str(cargo.parent) + os.pathsep + env.get('PATH', '')
if not subprocess.check_output([str(cargo.parent / 'rustc'), '--version'], env=env, text=True).startswith('rustc 1.97.1 '):
    raise SystemExit('Pinned Rust 1.97.1 required')
if env.get('CARGO_BUILD_TARGET'):
    raise SystemExit('Native executable required')
subprocess.run([str(cargo), 'build', '--offline', '--locked', '-p', 'orange-platform-files', '--example', 'security_probe'], cwd=ROOT, env=env, check=True, timeout=240)
stage_root = ROOT / '.local/security-probe'
stage_root.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='qualified-', dir=stage_root) as directory:
    stage = Path(directory)
    app = stage / 'MetadataProbe.app'
    executable = app / 'Contents/MacOS/MetadataProbe'
    executable.parent.mkdir(parents=True)
    binary = Path(env.get('CARGO_TARGET_DIR', ROOT / 'target')) / 'debug/examples/security_probe'
    shutil.copyfile(binary, executable)
    executable.chmod(0o755)
    resource = app / 'Contents/Resources/owned-source'
    resource.parent.mkdir()
    resource.write_bytes(b'owned original')
    (app / 'Contents/Info.plist').write_bytes(plistlib.dumps({
        'CFBundleIdentifier': 'dev.orangeagent.metadataProbe',
        'CFBundleExecutable': 'MetadataProbe', 'CFBundleName': 'MetadataProbe',
        'CFBundlePackageType': 'APPL', 'CFBundleVersion': '1', 'LSUIElement': True,
    }))
    entitlements = stage / 'sandbox.entitlements'
    entitlements.write_bytes(plistlib.dumps({'com.apple.security.app-sandbox': True}))
    subprocess.run(['codesign', '--force', '--sign', '-', '--entitlements', str(entitlements), str(app)], check=True, timeout=30)
    subprocess.run(['codesign', '--verify', '--strict', str(app)], check=True, timeout=30)
    signed = subprocess.run(['codesign', '--display', '--entitlements', ':-', str(app)], capture_output=True, check=True, timeout=30)
    assert plistlib.loads(signed.stdout)['com.apple.security.app-sandbox'] is True
    result = subprocess.run([str(executable), *(['--bundled-source'] if args.bundled_source else [])], capture_output=True, text=True, timeout=30)
    print(result.stdout, end='')
    if result.returncode:
        print(result.stderr)
        raise SystemExit(result.returncode)
    if args.bundled_source:
        assert 'source_has_quarantine=false' in result.stdout
        assert 'initial_target_label_preserved=true' in result.stdout
    else:
        assert 'quarantine_fields_equal=[true, false, true, true]' in result.stdout
    assert 'publication_policy=passed exact_target_bound=true source_unchanged=true' in result.stdout
print('PASS actual App Sandbox: preserve quarantine flags/agent/event and bind actual target timestamp; owned bytes and source metadata unchanged')
