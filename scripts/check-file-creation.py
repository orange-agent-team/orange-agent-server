#!/usr/bin/env python3
"""Actual CLI/SQLite/filesystem and local HTTP adapter; explicitly scripted model.

Each CLI invocation reopens the Host. Test approval is confined to owned fixtures.
No real inference, desktop OS consent or Windows support is certified here.
"""
import http.server
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import sqlite3
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--legacy-runtime',type=Path)
args=parser.parse_args()
legacy_binary=None
rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
env = os.environ.copy()
env['PATH'] = str(cargo.parent) + os.pathsep + env.get('PATH', '')
subprocess.run([str(cargo), 'build', '-p', 'orange-agent-runtime', '--example', 'local-agent', '--locked', '--offline'], cwd=ROOT, env=env, check=True, timeout=240)
metadata = json.loads(subprocess.check_output([str(cargo), 'metadata', '--no-deps', '--format-version', '1', '--locked', '--offline'], cwd=ROOT, env=env, text=True))
if env.get('CARGO_BUILD_TARGET'):
    raise SystemExit('CLI acceptance requires native build')
binary = Path(metadata['target_directory']) / 'debug/examples/local-agent'
if args.legacy_runtime:
    legacy=args.legacy_runtime.resolve()
    manifest_path=legacy/'release-manifest.json'
    if legacy.is_symlink() or hashlib.sha256(manifest_path.read_bytes()).hexdigest()!='cf33ccae01c04658b02188bab8268783828e9c651ec8066fc18aa6759d461400':
        raise SystemExit('Exact prior Runtime7e115c3 source artifact required')
    release=json.loads(manifest_path.read_text())
    if release['origin_commit']!='7e115c3aa197f307f43f0aa3aef5333fdb2d8479':
        raise SystemExit('Unexpected legacy origin')
    actual=set()
    for directory,folders,names in os.walk(legacy,followlinks=False):
        for name in folders:
            if (Path(directory)/name).is_symlink():
                raise SystemExit('Linked legacy source directory')
        folders[:]=[name for name in folders if name!='target']
        for name in names:
            file=Path(directory)/name
            if file.is_symlink() or not file.is_file():
                raise SystemExit('Irregular legacy source entry')
            actual.add(file.relative_to(legacy).as_posix())
    if actual!=set(release['content_sha256'])|{'release-manifest.json'}:
        raise SystemExit('Legacy source inventory mismatch')
    for relative,expected in release['content_sha256'].items():
        file=legacy/relative
        if file.is_symlink() or not file.is_file() or hashlib.sha256(file.read_bytes()).hexdigest()!=expected:
            raise SystemExit('Legacy source integrity mismatch: '+relative)
    legacy_env=env.copy();legacy_env['CARGO_TARGET_DIR']=str(ROOT/'.local/legacy-runtime-target')
    subprocess.run([str(cargo),'build','--manifest-path',str(legacy/'Cargo.toml'),'-p','orange-agent-runtime','--example','local-agent','--locked','--offline'],cwd=ROOT,env=legacy_env,check=True,timeout=300)
    legacy_binary=Path(legacy_env['CARGO_TARGET_DIR'])/'debug/examples/local-agent'
key_value = secrets.token_hex(32)
proof = 'owned-fixture-' + secrets.token_hex(16)
observations, failures = [], []

class ModelFixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        try:
            self.connection.settimeout(5)
            length = int(self.headers.get('Content-Length', '0'))
            assert 0 < length <= 256 * 1024
            assert self.headers.get('Authorization') == 'Bearer ' + key_value
            request = json.loads(self.rfile.read(length))
            if self.path == '/v1/chat/completions/input_tokens':
                payload = b'{"input_tokens":1}'
                kind = 'application/json'
            else:
                assert self.path == '/v1/chat/completions'
                assert any(t['function']['name'] == 'write_file' for t in request['tools'])
                index = len(observations)
                observations.append(request)
                if index == 0:
                    name, arguments = 'read_file', {'path': 'source.txt'}
                elif index == 1:
                    assert any(m.get('tool_call_id') == 'call-0' and proof in m['content'] for m in request['messages'])
                    name, arguments = 'write_file', {'path': '新建-文件.txt', 'content': proof}
                elif index == 2:
                    results = [json.loads(m['content']) for m in request['messages'] if m.get('tool_call_id') == 'call-1']
                    assert len(results) == 1 and results[0]['kind'] == 'write_file'
                    assert results[0]['data']['path'] == '新建-文件.txt'
                    name = None
                else:
                    raise AssertionError('unexpected model replay')
                if name:
                    delta = {'role': 'assistant', 'tool_calls': [{'index': 0, 'id': 'call-' + str(index), 'type': 'function', 'function': {'name': name, 'arguments': json.dumps(arguments, ensure_ascii=False)}}]}
                    finish = 'tool_calls'
                else:
                    delta, finish = {'role': 'assistant', 'content': 'fixture-complete'}, 'stop'
                rid = 'fixture-response-' + str(index)
                events = [{'id': rid, 'choices': [{'index': 0, 'delta': delta, 'finish_reason': None}]}, {'id': rid, 'choices': [{'index': 0, 'delta': {}, 'finish_reason': finish}]}]
                payload = (''.join('data: ' + json.dumps(e, ensure_ascii=False) + '\n\n' for e in events) + 'data: [DONE]\n\n').encode()
                kind = 'text/event-stream; charset=utf-8'
            self.send_response(200)
            self.send_header('Content-Type', kind)
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except Exception as error:
            failures.append(type(error).__name__)
            self.send_error(400, 'explicit fixture refused request')

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), ModelFixture)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory(prefix='orange-create-cli-') as directory:
        base = Path(directory)
        private, workspace = base / 'private', base / 'workspace'
        private.mkdir(mode=0o700)
        workspace.mkdir()
        assets = base / 'excluded-model-assets'
        assets.mkdir(mode=0o755)
        assets.chmod(0o755)
        (workspace / 'source.txt').write_text(proof)
        key = private / 'key'
        key.write_text(key_value)
        key.chmod(0o600)
        config_path, database = private / 'config.json', private / 'journal.sqlite'
        config = {'run': {'owner': 'create-owner', 'run_id': 'create-run', 'policy': 'fixture-policy', 'environment': 'pending', 'model_version': 'create-fixture', 'max_steps': 12, 'max_invocations': 6, 'max_output_tokens': 512, 'deadline_unix': int(time.time()) + 120}, 'grant_id': 'read-grant', 'workspace': str(workspace), 'journal': str(database), 'private_roots': [str(private), str(assets)], 'model_endpoint': 'http://127.0.0.1:' + str(server.server_port) + '/', 'model_key_file': str(key), 'context_tokens': 2048, 'prompt': 'Read source.txt and propose a new file after separate approval'}
        def save():
            config_path.write_text(json.dumps(config))
            config_path.chmod(0o600)
        active_binary = legacy_binary or binary
        def invoke(command, *arguments, failed=False):
            result = subprocess.run([str(active_binary), str(config_path), command, *arguments], cwd=ROOT, env=env, capture_output=True, text=True, timeout=20)
            if failed:
                assert result.returncode != 0, 'invalid command accepted'
                return None
            if result.returncode:
                raise RuntimeError('CLI ' + command + ' failed: ' + result.stderr[:500])
            return json.loads(result.stdout)
        save()
        # An exclusion is not necessarily a writable private state directory.
        # Broad root selection must still fail even before durable consent.
        config['workspace'] = str(base)
        save()
        invoke('grant-info', failed=True)
        config['workspace'] = str(workspace)
        save()
        config['run']['environment'] = invoke('grant-info')['environment']
        save()
        consent = invoke('authorize-create', 'explicit-create-request')
        assert invoke('authorize-create', 'explicit-create-request') == consent
        config['creation_consent_id'] = consent['consent_id']
        save()
        assert invoke('start')['phase'] == 'ready'
        pending = invoke('step')['pending'][0]
        invoke('step', failed=True)
        invoke('approve', pending['operation_id'])
        assert invoke('step')['phase'] == 'ready'
        pending = invoke('step')['pending'][0]
        target = workspace / '新建-文件.txt'
        assert not target.exists()
        assert invoke('show')['pending'][0] == pending
        invoke('approve', pending['operation_id'], failed=True)
        invoke('approve-create', pending['operation_id'], '0' * 64, failed=True)
        invoke('step', failed=True)
        with sqlite3.connect(database) as db:
            assert db.execute('SELECT count(*) FROM approvals WHERE operation_id=?', (pending['operation_id'],)).fetchone()[0] == 0
        invoke('approve-create', pending['operation_id'], pending['argument_digest'])
        if legacy_binary:
            with sqlite3.connect(database) as db:
                assert db.execute('PRAGMA user_version').fetchone()[0] == 6
                assert db.execute('SELECT count(*) FROM approvals').fetchone()[0] == 2
                assert db.execute("SELECT count(*) FROM operations WHERE state='intended'").fetchone()[0] == 1
            assert not target.exists()
            active_binary = binary
        assert invoke('step')['phase'] == 'ready'
        if legacy_binary:
            with sqlite3.connect(database) as db:
                assert db.execute('PRAGMA user_version').fetchone()[0] == 20
                assert db.execute("SELECT count(*) FROM runtime_tool_catalogs WHERE origin='legacy_builtin_v1'").fetchone()[0] == 2
                assert db.execute('SELECT count(*) FROM approvals').fetchone()[0] == 2
                assert db.execute('SELECT count(*) FROM runtime_tool_approvals').fetchone()[0] == 1

        assert target.read_text() == proof
        complete = invoke('step')
        assert complete['phase'] == 'completed' and complete['steps'] == 5
        assert invoke('step') == complete and len(observations) == 3
        with sqlite3.connect(database) as db:
            assert db.execute("SELECT count(*) FROM operations WHERE tool_version='write_file.v1' AND state='succeeded' AND context_applied=1").fetchone()[0] == 1
            assert db.execute('PRAGMA user_version').fetchone()[0] == 20
        target.unlink()
        invoke('show', failed=True)
        invoke('step', failed=True)
        revoked = invoke('revoke-create', consent['version'])
        assert not revoked['active']
        invoke('authorize-create', 'explicit-create-request', failed=True)
        assert len(observations) == 3 and not failures and not target.exists()
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
print('PASS actual CLI/HTTP adapter/SQLite/source read/new Unicode file/exact approval/reopen/no replay/revocation; model responses and count are explicit fixtures, not inference or OS-consent certification')

if legacy_binary:
    print('PASS actual fixed Runtime7e115c3 schema6 pending approved creation -> current current-schema snapshot/approval adoption; no extra approval or effect replay. Desktop OS access restoration is separately verified.')
