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
args = parser.parse_args()
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
                    return None
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
            assert (workspace / 'note.txt').read_text() == expected
            assert 'note.txt' in {p.name for p in workspace.iterdir()}
            assert len(observations['positive']) == index + 1
        complete = invoke('step')
        assert complete['phase'] == 'completed' and (complete['steps'], complete['invocations']) == (7, 4)
        assert invoke('step') == complete and len(observations['positive']) == 4
        with sqlite3.connect(database) as db:
            assert db.execute('PRAGMA user_version').fetchone()[0] == 17
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
