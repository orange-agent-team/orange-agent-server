#!/usr/bin/env python3
"""Actual CLI, HTTP adapter, SQLite and files; model responses are scripted.

Every command starts a new Host. Approvals only concern owned temporary files.
This is not real inference, OS-picker, production signing or GPU certification.
"""
import hashlib
import argparse
import atexit
import http.server
import json
import os
from journal_schema import verify_current_journal
from pathlib import Path
import platform
import plistlib
import secrets
import shutil
import sqlite3
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--sandboxed', action='store_true', help='Execute the same real CLI in a signed App Sandbox; fixtures are created by this outside process')
parser.add_argument('--legacy-runtime', type=Path, help='Exact supported fixed source manifest; migrate actual successful and approved edits')
args = parser.parse_args()
if args.sandboxed and args.legacy_runtime:
    raise SystemExit('Legacy compatibility and sandbox qualification are separate invocations')
if platform.system() != 'Darwin':
    raise SystemExit('Actual editing acceptance currently requires the implemented Apple backend')
rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
env = os.environ.copy()
env['PATH'] = str(cargo.parent) + os.pathsep + env.get('PATH', '')
if not subprocess.check_output([str(cargo.parent / 'rustc'), '--version'], env=env, text=True).startswith('rustc 1.97.1 '):
    raise SystemExit('Pinned Rust 1.97.1 required')
subprocess.run([str(cargo), 'build', '-p', 'orange-agent-runtime', '--example', 'local-agent', '--locked', '--offline'], cwd=ROOT, env=env, check=True, timeout=240)
metadata = json.loads(subprocess.check_output([str(cargo), 'metadata', '--no-deps', '--format-version', '1', '--locked', '--offline'], cwd=ROOT, env=env, text=True))
if env.get('CARGO_BUILD_TARGET'):
    raise SystemExit('Native CLI acceptance required')
binary = Path(metadata['target_directory']) / 'debug/examples/local-agent'
current_binary = binary
legacy_binary = None
legacy_schema = None
if args.legacy_runtime:
    legacy = args.legacy_runtime.resolve()
    release_path = legacy / 'release-manifest.json'
    release = json.loads(release_path.read_text())
    known = {
        '3ce40db6958ccd0bd499cc78c4882157e5e6aadd': (28, '9c147932f4ff0a34e3571981dda15dfa22994ea1655c0d9638e05ddbc1018e0e'),
        '64748cd25940b9bef28d1efd90488a8e8ab76684': (16, '5f0b1a2c50b0c95ab288a00817926dbc3fa3f928b451547342228a50ccff1266'),
        'a2cdafb95fab90bc1ded82d4a4c061be40463d35': (17, '874a60c4b4364ac54f6accb6a004a9975f7476a78f6c031872ab3d8802af7150'),
        '84d797ca3f2eefdfd675e5d905664add292b713a': (18, '786b0156028da1cccfd1f3f197427b37bfc65a905e6f774e4b2afa074bdb85c6'),
        'c60c50290ea901ebeea2a4c0cb4b307d79f3cb4f': (19, '31608d95b636bd9b902484d33aabdbd508120c370d1d3ea7aaea445b81bfc921'),
        'e77a61e2a213b817770e7e01e38a742043df86b6': (20, '9be2a6f56d187b9bdf48242d75859b2a3e0b96d6eb03b482cdbddf0fa1630070'),
        '48a7aae275c2467ef865a9301135945a889d7f2d': (21, '482c13976cadc36451ad78c73a555e427c8900c5bc60f31afe989c57ba219840'),
        '303c9cbd3f7f5692a7b46f8b2119c3ccdd3f2efc': (22, 'a630dea610f3bac028c35d98f1bdd0bc8762874977673eba5cdc89233d27b5d5'),
        'ffb6d402aea406368caf7bef46ca1f8728249788': (23, '70994442163b30930f70e74da173b4392dad399c952500ace166a4473d8b44bb'),
        '6784cf368a5d1a05140eaba4ec12242c95a8a14b': (24, '394895ee1a19b8912bdc6ca1ee78707edb9905427c57f7490d030b0f5d5ed3e6'),
        '71941fdb165b8d85563206788b69651992280d85': (25, '7f5a71e707a19771ca004c8332236075e1da67d918a3cbebf4770212c8097e9e'),
        '0de3f2ccc33b70bebfc8858ee7f5ab41ea4b9174': (26, '283ed52467292292e240b7f3220dee0196a7ab262d425ad3508d0ddd1ca175ef'),
        'fb69ea634f67e6ec6d624f9efb566ce89759c39d': (27, '58e9600c22bdd56c50df3c8687437591931790318df80c2c270fb275c3940712'),
    }
    pinned = known.get(release.get('origin_commit'))
    if legacy.is_symlink() or pinned is None or hashlib.sha256(release_path.read_bytes()).hexdigest() != pinned[1]:
        raise SystemExit('Exact supported schema16 through schema28 Runtime source manifest required')
    legacy_schema = pinned[0]
    actual = set()
    for directory, folders, names in os.walk(legacy, followlinks=False):
        if any((Path(directory) / name).is_symlink() for name in folders):
            raise SystemExit('Linked legacy source directory')
        folders[:] = [name for name in folders if name != 'target']
        for name in names:
            path = Path(directory) / name
            if path.is_symlink() or not path.is_file():
                raise SystemExit('Irregular legacy source entry')
            relative = path.relative_to(legacy).as_posix()
            actual.add(relative)
            if relative != 'release-manifest.json' and hashlib.sha256(path.read_bytes()).hexdigest() != release['content_sha256'].get(relative):
                raise SystemExit('Legacy source bytes differ')
    assert actual == set(release['content_sha256']) | {'release-manifest.json'}
    legacy_env = env.copy()
    legacy_env['CARGO_TARGET_DIR'] = str(ROOT / ('.local/schema' + str(legacy_schema) + '-compat-target'))
    subprocess.run([str(cargo), 'build', '--manifest-path', str(legacy / 'Cargo.toml'), '-p', 'orange-agent-runtime', '--example', 'local-agent', '--locked', '--offline'], env=legacy_env, check=True, timeout=300)
    legacy_binary = Path(legacy_env['CARGO_TARGET_DIR']) / 'debug/examples/local-agent'
    binary = legacy_binary
fixture_parent = None
if args.sandboxed:
    parent = ROOT / '.local/sandbox-editing'
    parent.mkdir(parents=True, exist_ok=True)
    package = tempfile.TemporaryDirectory(prefix='qualified-', dir=parent)
    atexit.register(package.cleanup)
    app = Path(package.name) / 'EditMetadataProbe.app'
    destination = app / 'Contents/MacOS/EditMetadataProbe'
    destination.parent.mkdir(parents=True)
    shutil.copyfile(binary, destination)
    destination.chmod(0o755)
    identifier = 'dev.orangeagent.editMetadataProbe'
    (app / 'Contents/Info.plist').write_bytes(plistlib.dumps({
        'CFBundleIdentifier': identifier, 'CFBundleExecutable': 'EditMetadataProbe',
        'CFBundlePackageType': 'APPL', 'CFBundleVersion': '1', 'LSUIElement': True,
    }))
    entitlement = Path(package.name) / 'sandbox.entitlements'
    entitlement.write_bytes(plistlib.dumps({'com.apple.security.app-sandbox': True, 'com.apple.security.network.client': True}))
    subprocess.run(['codesign', '--force', '--sign', '-', '--entitlements', str(entitlement), str(app)], check=True, timeout=30)
    subprocess.run(['codesign', '--verify', '--strict', str(app)], check=True, timeout=30)
    signed = subprocess.run(['codesign', '--display', '--entitlements', ':-', str(app)], capture_output=True, check=True, timeout=30)
    assert plistlib.loads(signed.stdout)['com.apple.security.app-sandbox'] is True
    binary = destination
    # Bootstrap only the dedicated test container. A usage rejection is the
    # expected result, not an editing-test pass or a permission bypass.
    bootstrap = subprocess.run([str(binary)], capture_output=True, text=True, env=env, timeout=30)
    assert bootstrap.returncode != 0 and 'usage: local-agent' in bootstrap.stderr
    fixture_parent = Path.home() / 'Library/Containers' / identifier / 'Data/tmp'
    assert fixture_parent.is_dir(), 'Dedicated sandbox container was not initialized'
key_value = secrets.token_hex(32)
observations, failures = {}, []
old = 'owned-source-' + secrets.token_hex(8) + '\n'
middle, final = 'reviewed middle\n', 'reviewed final\n'


def patch(path, before, after):
    return {'path': path, 'expected_sha256': hashlib.sha256(before.encode()).hexdigest(),
            'patch': '@@ -1 +1 @@\n-' + before.rstrip('\n') + '\n+' + after.rstrip('\n') + '\n'}


class ModelFixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            self.connection.settimeout(5)
            length = int(self.headers.get('Content-Length', '0'))
            assert 0 < length <= 256 * 1024
            assert self.headers.get('Authorization') == 'Bearer ' + key_value
            request = json.loads(self.rfile.read(length))
            if self.path == '/v1/chat/completions/input_tokens':
                payload, kind = b'{"input_tokens":1}', 'application/json'
            else:
                assert self.path == '/v1/chat/completions'
                scenario = request['model']
                seen = observations.setdefault(scenario, [])
                index = len(seen)
                seen.append(request)
                names = {t['function']['name'] for t in request['tools']}
                assert ('apply_patch' in names) == (scenario != 'readonly')
                for message in request['messages']:
                    if message['role'] == 'tool':
                        assert 'retained_original' not in message['content']
                        assert '.orange-agent-tmp-' not in message['content']
                if scenario == 'positive':
                    if index == 0:
                        name, arguments = 'read_file', {'path': 'note.txt'}
                    elif index == 1:
                        assert any(m.get('tool_call_id') == 'call-0' and old.strip() in m['content'] for m in request['messages'])
                        name, arguments = 'apply_patch', patch('NOTE.TXT', old, middle)
                    elif index == 2:
                        assert any(m.get('tool_call_id') == 'call-1' and json.loads(m['content'])['kind'] == 'apply_patch' for m in request['messages'])
                        name, arguments = 'apply_patch', patch('note.txt', middle, final)
                    elif index == 3:
                        name, arguments = None, None
                    else:
                        raise AssertionError('unexpected model replay')
                elif scenario == 'combined':
                    assert 'write_file' in names
                    if index == 0:
                        name, arguments = 'write_file', {'path': '新建.txt', 'content': old}
                    elif index == 1:
                        name, arguments = 'apply_patch', patch('新建.txt', old, final)
                    elif index == 2:
                        name, arguments = None, None
                    else:
                        raise AssertionError('unexpected combined replay')
                elif scenario in {'readonly', 'revoked', 'unknown', 'capture-unknown'}:
                    assert index == 0
                    name, arguments = 'apply_patch', patch('note.txt', old, final)
                else:
                    raise AssertionError('unknown fixture profile')
                rid = 'fixture-response-' + str(index)
                if name:
                    delta = {'role': 'assistant', 'tool_calls': [{'index': 0, 'id': 'call-' + str(index), 'type': 'function', 'function': {'name': name, 'arguments': json.dumps(arguments, ensure_ascii=False)}}]}
                    finish = 'tool_calls'
                else:
                    delta, finish = {'role': 'assistant', 'content': 'fixture-complete'}, 'stop'
                events = [{'id': rid, 'choices': [{'index': 0, 'delta': delta, 'finish_reason': None}]}, {'id': rid, 'choices': [{'index': 0, 'delta': {}, 'finish_reason': finish}]}]
                payload = (''.join('data: ' + json.dumps(event, ensure_ascii=False) + '\n\n' for event in events) + 'data: [DONE]\n\n').encode()
                kind = 'text/event-stream; charset=utf-8'
            self.send_response(200)
            self.send_header('Content-Type', kind)
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except Exception as error:
            failures.append(type(error).__name__)
            self.send_error(400, 'owned model fixture rejected request')


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), ModelFixture)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
summary = []
try:
    with tempfile.TemporaryDirectory(prefix='orange-edit-cli-', dir=fixture_parent) as directory:
        base = Path(directory)

        def fixture(scenario):
            root = base / scenario
            private, workspace = root / 'private', root / 'workspace'
            private.mkdir(parents=True, mode=0o700)
            workspace.mkdir()
            (workspace / 'note.txt').write_text(old)
            if args.sandboxed:
                names = subprocess.check_output(['/usr/bin/xattr', str(workspace / 'note.txt')], text=True).splitlines()
                assert 'com.apple.quarantine' not in names, 'Outside-created fixture must genuinely start without quarantine'
            key = private / 'key'
            key.write_text(key_value)
            key.chmod(0o600)
            config_path, database = private / 'config.json', private / 'journal.sqlite'
            config = {'run': {'owner': 'edit-owner', 'run_id': scenario + '-run', 'policy': 'fixture-policy', 'environment': 'pending', 'model_version': scenario, 'max_steps': 20, 'max_invocations': 8, 'max_output_tokens': 512, 'deadline_unix': int(time.time()) + 240}, 'grant_id': 'read-grant', 'workspace': str(workspace), 'journal': str(database), 'private_roots': [str(private)], 'model_endpoint': f'http://127.0.0.1:{server.server_port}/', 'model_key_file': str(key), 'context_tokens': 2048, 'prompt': 'Owned CLI edit acceptance fixture'}

            def save():
                config_path.write_text(json.dumps(config))
                config_path.chmod(0o600)

            def invoke(command, *arguments, failed=False):
                result = subprocess.run([str(binary), str(config_path), command, *arguments], cwd=ROOT, env=env, capture_output=True, text=True, timeout=25)
                if failed:
                    assert result.returncode != 0, 'invalid command accepted'
                    return result
                if result.returncode:
                    raise RuntimeError('CLI ' + command + ' failed: ' + result.stderr[:500])
                return json.loads(result.stdout)

            save()
            config['run']['environment'] = invoke('grant-info')['environment']
            save()
            return config, save, invoke, workspace, database, key

        config, save, invoke, workspace, database, key = fixture('positive')
        # The tested APFS aliases this spelling. If a future test volume does
        # not, use its real literal path instead of claiming alias certification.
        assert (workspace / 'NOTE.TXT').exists(), 'this scenario explicitly certifies actual native case aliasing'
        consent = invoke('authorize-edit', 'explicit-edit')
        assert invoke('authorize-edit', 'explicit-edit') == consent
        config['editing_consent_id'] = consent['consent_id']
        save()
        assert invoke('start')['phase'] == 'ready'
        invoke('start', 'unexpected', failed=True)
        pending = invoke('step')['pending'][0]
        invoke('approve', pending['operation_id'])
        assert invoke('step')['phase'] == 'ready'
        for index, expected in [(1, middle), (2, final)]:
            pending = invoke('step')['pending'][0]
            invoke('approve', pending['operation_id'], failed=True)
            preview = invoke('preview-edit', pending['operation_id'])
            assert invoke('preview-edit', pending['operation_id']) == preview
            invoke('approve-edit', pending['operation_id'], pending['argument_digest'], failed=True)
            invoke('step', failed=True)
            with sqlite3.connect(database) as db:
                assert db.execute('SELECT count(*) FROM approvals WHERE operation_id=?', (pending['operation_id'],)).fetchone()[0] == 0
            invoke('approve-edit', pending['operation_id'], preview['preview_digest'])
            if legacy_binary and index == 2:
                with sqlite3.connect(database) as db:
                    assert db.execute('PRAGMA user_version').fetchone()[0] == legacy_schema
                    assert db.execute('SELECT count(*) FROM patch_version_edges').fetchone()[0] == 1
                    if legacy_schema == 16:
                        assert db.execute("SELECT count(*) FROM sqlite_master WHERE name LIKE 'patch_capture_%'").fetchone()[0] == 0
                        saved_capture_receipts = []
                    else:
                        saved_capture_receipts = db.execute('SELECT * FROM patch_capture_receipts ORDER BY operation_id').fetchall()
                        assert len(saved_capture_receipts) == 1
                        assert db.execute("SELECT count(*) FROM sqlite_master WHERE name='patch_exports'").fetchone()[0] == (0 if legacy_schema == 17 else 1)
                    saved_approvals = db.execute('SELECT * FROM editing_approvals ORDER BY operation_id').fetchall()
                binary = current_binary
            actual = invoke('step')
            if actual['phase'] != 'ready' and args.sandboxed:
                report = ROOT / '.local/sandbox-editing' / ('failure-' + secrets.token_hex(8))
                report.mkdir(mode=0o700)
                with sqlite3.connect(database) as source_db, sqlite3.connect(report / 'journal.sqlite') as copy_db:
                    source_db.backup(copy_db)
                    details = {
                        'phase': actual['phase'], 'failure': actual.get('failure'),
                        'steps': source_db.execute('SELECT step,body FROM editing_publication_steps ORDER BY rowid').fetchall(),
                        'files': [{'name': f.name, 'bytes': f.stat().st_size,
                                   'sha256': hashlib.sha256(f.read_bytes()).hexdigest(),
                                   'attributes': subprocess.check_output(['/usr/bin/xattr', str(f)], text=True).splitlines()}
                                  for f in workspace.iterdir() if f.is_file()],
                    }
                shutil.copytree(workspace, report / 'workspace')
                (report / 'diagnosis.json').write_text(json.dumps(details, indent=2) + '\n')
                print(json.dumps({'owned_diagnostic': str(report), **details}), flush=True)
            assert actual['phase'] == 'ready'
            if legacy_binary and index == 2:
                with sqlite3.connect(database) as db:
                    current_schema = verify_current_journal(db)
                    assert db.execute('SELECT * FROM editing_approvals ORDER BY operation_id').fetchall() == saved_approvals
                    assert db.execute('SELECT count(*) FROM patch_capture_witnesses').fetchone()[0] == 0
                    assert db.execute('SELECT count(*) FROM patch_capture_receipts').fetchone()[0] == 1 + len(saved_capture_receipts)
                    for saved in saved_capture_receipts:
                        assert db.execute('SELECT * FROM patch_capture_receipts WHERE operation_id=?', (saved[0],)).fetchone() == saved
                    assert db.execute('SELECT count(*) FROM patch_exports').fetchone()[0] == 0
                    assert db.execute('SELECT count(*) FROM export_writer_bindings').fetchone()[0] == 0
                def durable_bytes():
                    return {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in [database, database.with_name(database.name + '-wal')] if p.exists()}
                before_rejected_open = durable_bytes()
                binary = legacy_binary
                rejected = invoke('status', failed=True)
                assert 'UnsupportedVersion' in rejected.stderr or 'unsupported journal format' in rejected.stderr
                assert durable_bytes() == before_rejected_open, 'old pinned binary changed newer data'
                binary = current_binary
            assert (workspace / 'note.txt').read_text() == expected
            assert 'note.txt' in {p.name for p in workspace.iterdir()}
            assert len(observations['positive']) == index + 1
        complete = invoke('step')
        assert complete['phase'] == 'completed' and (complete['steps'], complete['invocations']) == (7, 4)
        assert invoke('step') == complete and len(observations['positive']) == 4
        with sqlite3.connect(database) as db:
            current_schema = verify_current_journal(db)
            assert db.execute('SELECT count(*) FROM patch_version_edges').fetchone()[0] == 2
            assert db.execute('SELECT count(*) FROM runtime_tool_approvals').fetchone()[0] == 3
            first = db.execute('SELECT operation_id FROM operations WHERE tool_version=? ORDER BY rowid', ('apply_patch.v1',)).fetchone()[0]
        key.unlink()  # Metadata and recovery inspection require no model secret.
        status = invoke('status')
        assert status['phase'] == 'completed' and not {'pending', 'final_text', 'messages'} & status.keys()
        assert invoke('inspect-edit', first)['journal']['execution_state'] == 'succeeded'
        (workspace / 'note.txt').unlink()
        invoke('show', failed=True)
        assert invoke('status')['phase'] == 'completed'
        assert invoke('inspect-edit', first)['target']['kind'] == 'missing'
        moved = workspace.parent / 'moved-workspace'
        workspace.rename(moved)  # Preserve owned originals; remove only the old location.
        assert invoke('status')['phase'] == 'completed'
        invoke('inspect-edit', first, failed=True)
        assert len(observations['positive']) == 4
        summary.append({'scenario': 'positive', 'model_calls': 4, 'actual_edits': 2, 'reopen_no_replay': True})
        if legacy_binary:
            summary.append({'scenario': f"schema{legacy_schema}-to{current_schema}", 'actual_old_success_and_approved_edit': True, 'approval_preserved': True, 'no_old_capture_evidence_invented': True, 'historical_source_and_pending_edit_validated': True, 'old_binary_rejected_without_data_mutation': True})

        config, save, invoke, workspace, database, _ = fixture('combined')
        create = invoke('authorize-create', 'explicit-create')
        edit = invoke('authorize-edit', 'explicit-edit')
        config.update(creation_consent_id=create['consent_id'], editing_consent_id=edit['consent_id'])
        save()
        invoke('start')
        pending = invoke('step')['pending'][0]
        invoke('approve-create', pending['operation_id'], pending['argument_digest'])
        invoke('step')
        pending = invoke('step')['pending'][0]
        preview = invoke('preview-edit', pending['operation_id'])
        invoke('approve-edit', pending['operation_id'], preview['preview_digest'])
        assert invoke('pending-edits') == {'operation_ids': [], 'next_before': None}
        invoke('step')
        assert invoke('step')['phase'] == 'completed'
        assert (workspace / '新建.txt').read_text() == final
        assert len(observations['combined']) == 3
        summary.append({'scenario': 'combined', 'model_calls': 3, 'actual_creates': 1, 'actual_edits': 1})

        config, save, invoke, workspace, database, _ = fixture('readonly')
        edit = invoke('authorize-edit', 'unused-independent-consent')
        invoke('start')
        config['editing_consent_id'] = edit['consent_id']
        save()
        invoke('start', failed=True)
        invoke('step', failed=True)
        config.pop('editing_consent_id')
        save()
        # The real adapter can reject the unavailable tool itself or return it
        # to the engine, but neither may commit a tool operation or any effect.
        result = subprocess.run([str(binary), str(database.parent / 'config.json'), 'step'], cwd=ROOT, env=env, capture_output=True, text=True, timeout=25)
        assert result.returncode != 0 or json.loads(result.stdout)['phase'] == 'recovery_required'
        with sqlite3.connect(database) as db:
            assert db.execute('SELECT count(*) FROM operations').fetchone()[0] == 0
            assert db.execute('SELECT count(*) FROM runtime_edit_capabilities').fetchone()[0] == 0
        assert (workspace / 'note.txt').read_text() == old and len(observations['readonly']) == 1
        summary.append({'scenario': 'readonly', 'write_effects': 0, 'capability_upgrade': False})

        config, save, invoke, workspace, database, key = fixture('revoked')
        edit = invoke('authorize-edit', 'explicit-edit')
        config['editing_consent_id'] = edit['consent_id']
        save()
        invoke('start')
        pending = invoke('step')['pending'][0]
        preview = invoke('preview-edit', pending['operation_id'])
        assert not invoke('revoke-edit', edit['version'])['active']
        invoke('approve-edit', pending['operation_id'], preview['preview_digest'], failed=True)
        invoke('step', failed=True)
        invoke('authorize-edit', 'explicit-edit', failed=True)
        key.unlink()
        assert invoke('status')['phase'] == 'tools_pending'
        assert invoke('cancel')['phase'] == 'cancelled'
        assert (workspace / 'note.txt').read_text() == old
        summary.append({'scenario': 'revoked', 'write_effects': 0, 'metadata_cancel_without_write_restore': True})

        config, save, invoke, workspace, database, key = fixture('unknown')
        edit = invoke('authorize-edit', 'explicit-edit')
        config['editing_consent_id'] = edit['consent_id']
        save()
        invoke('start')
        pending = invoke('step')['pending'][0]
        preview = invoke('preview-edit', pending['operation_id'])
        invoke('approve-edit', pending['operation_id'], preview['preview_digest'])
        with sqlite3.connect(database) as db:
            db.execute("CREATE TRIGGER fixture_receipt_failure BEFORE INSERT ON patch_version_edges BEGIN SELECT RAISE(ABORT,'owned receipt fault');END;")
        invoke('step')
        assert (workspace / 'note.txt').read_text() == final
        assert invoke('status')['phase'] == 'recovery_required'
        invoke('step', failed=True)
        key.unlink()
        inspected = invoke('inspect-edit', pending['operation_id'])
        assert inspected['journal']['execution_state'] == 'unknown'
        assert inspected['target']['matches_result_content']
        assert inspected['original']['matches_original_content']
        assert invoke('cancel')['phase'] == 'cancelled'
        assert len(observations['unknown']) == 1
        assert invoke('pending-edits') == {'operation_ids': [pending['operation_id']], 'next_before': None}
        assert invoke('pending-edits', pending['operation_id']) == {'operation_ids': [], 'next_before': None}
        invoke('pending-edits', 'invalid cursor', failed=True)
        hidden_workspace = workspace.with_name(workspace.name + '-closed')
        workspace.rename(hidden_workspace)
        assert invoke('pending-edits') == {'operation_ids': [pending['operation_id']], 'next_before': None}
        assert (hidden_workspace / 'note.txt').read_text() == final
        with sqlite3.connect(database) as db:
            assert db.execute('SELECT count(*) FROM patch_version_edges').fetchone()[0] == 0
            assert db.execute("SELECT count(*) FROM operations WHERE state='unknown'").fetchone()[0] == 1
        summary.append({'scenario': 'unknown', 'actual_effect_retained': True, 'automatic_replay': False, 'bounded_metadata_discovery_without_workspace_or_key': True})
        config, save, invoke, workspace, database, key = fixture('capture-unknown')
        edit = invoke('authorize-edit', 'explicit-capture-edit')
        config['editing_consent_id'] = edit['consent_id']
        save()
        invoke('start')
        pending = invoke('step')['pending'][0]
        preview = invoke('preview-edit', pending['operation_id'])
        invoke('approve-edit', pending['operation_id'], preview['preview_digest'])
        with sqlite3.connect(database) as db:
            db.execute("CREATE TRIGGER fixture_capture_failure BEFORE INSERT ON editing_publication_steps WHEN NEW.step=4 BEGIN SELECT RAISE(ABORT,'owned captured-stage fault');END;")
        invoke('step')
        assert not (workspace / 'note.txt').exists()
        assert invoke('status')['phase'] == 'recovery_required'
        inspected = invoke('inspect-edit', pending['operation_id'])
        assert inspected['journal']['last_durable_stage'] == 'capture_intent'
        assert inspected['original']['reviewed_original_inode'] and inspected['original']['matches_original_content']
        assert inspected['staged']['matches_result_content']
        with sqlite3.connect(database) as db:
            assert db.execute('SELECT count(*) FROM patch_capture_receipts').fetchone()[0] == 0
            assert db.execute('SELECT count(*) FROM patch_capture_witnesses').fetchone()[0] == (1 if args.sandboxed else 0)
        invoke('step', failed=True)
        assert len(observations['capture-unknown']) == 1
        assert not (workspace / 'note.txt').exists(), 'inspection/reopen must not silently restore or publish'
        summary.append({'scenario': 'capture-unknown', 'original_recognized_after_reopen': True, 'no_capture_receipt_invented': True, 'no_automatic_restore_or_replay': True})
        assert not failures, failures
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
    assert not thread.is_alive(), 'owned fixture server did not stop'
print(json.dumps({'result': 'PASS', 'app_sandbox': args.sandboxed, 'outside_created_unlabelled_sources': args.sandboxed, 'scope': 'actual CLI/HTTP adapter/SQLite/files; scripted model responses, no inference or OS picker certification', 'scenarios': summary, 'fixture_server_stopped': True}, indent=2))
