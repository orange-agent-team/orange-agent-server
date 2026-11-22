"""Owned fixture for a real AU-07 run. Only the exact expected change may be approved."""
import hashlib
import json
import sqlite3
import uuid


def verify(config, workspace, persist, invoke):
    marker = uuid.uuid4().hex
    original = ('status=before\nproof=' + marker + '\n').encode()
    wanted = original.replace(b'status=before', b'status=after')
    source = workspace / 'note.txt'
    source.write_bytes(original)
    patch = '@@ -1 +1 @@\n-status=before\n+status=after\n'
    original_sha = hashlib.sha256(original).hexdigest()
    result_sha = hashlib.sha256(wanted).hexdigest()
    pin = next(v['pin'] for v in invoke('skills') if v['pin']['id'] == 'orange.reviewed-patch')
    config.update(skill=pin, prompt='First call read_file with only {"path":"note.txt"}. Then call apply_patch once with '
                  + json.dumps({'path':'note.txt','expected_sha256':'SHA_FROM_READ','patch':patch})
                  + '. Replace SHA_FROM_READ with the actual read digest. Keep the second line intact. After the approved edit is committed, reply with status=after.')
    config['run']['run_id'] = uuid.uuid4().hex
    config['run']['max_output_tokens'] = 512
    persist()
    config['skill_activation_revision'] = invoke('enable-skill','0')['revision']
    persist()
    invoke('start', expected_denial='separate edit consent required')
    config['editing_consent_id'] = invoke('authorize-edit','owned-patch-skill')['consent_id']
    persist()
    view = invoke('start')
    if invoke('skill-binding') != pin:
        raise RuntimeError('Run lacks the exact reviewed patch template')
    approved = []
    for _ in range(12):
        if view['phase'] == 'completed':
            break
        if view['phase'] not in ('ready','tools_pending'):
            raise RuntimeError('Patch skill stopped in '+view['phase']+' ('+str(view.get('failure'))+')')
        for item in view.get('pending',[]):
            request = item['request']
            tool = request['tool']
            operation = item['operation_id']
            after_patch = 'apply_patch' in approved
            expected_content = wanted if after_patch else original
            if source.read_bytes() != expected_content:
                raise RuntimeError('Source differs from the expected stage of the approved edit')
            invoke('step',expect_error=True)
            if tool == 'read_file' and request['arguments']['path'] == source.name and approved in ([], ['read_file','apply_patch']):
                invoke('approve',operation)
            elif approved == ['read_file'] and tool == 'apply_patch' and request['arguments'] == {
                'path':source.name,'expected_sha256':original_sha,'patch':patch
            }:
                preview = invoke('preview-edit',operation)
                if (preview['preview']['original_sha256'],preview['preview']['result_sha256']) != (original_sha,result_sha):
                    raise RuntimeError('Preview differs from the selected source and expected result')
                invoke('approve-edit',operation,'0'*64,expected_denial='Error:')
                if source.read_bytes() != original:
                    raise RuntimeError('Rejected preview approval modified the file')
                invoke('approve-edit',operation,preview['preview_digest'])
            else:
                raise RuntimeError('Refused an unexpected real tool or proposed patch')
            approved.append(tool)
        view = invoke('step')
    if approved not in (['read_file','apply_patch'], ['read_file','apply_patch','read_file']) or view['phase'] != 'completed' or 'status=after' not in (view.get('final_text') or ''):
        raise RuntimeError('Expected a completed patch report after the approved edit')
    if source.read_bytes() != wanted or invoke('show') != view or invoke('step') != view:
        raise RuntimeError('Incorrect file content or changed response on reopen')
    with sqlite3.connect(config['journal']) as db:
        receipts=db.execute("SELECT result_json FROM operations WHERE run_id=? AND state='succeeded' AND context_applied=1 AND tool_version='apply_patch.v1'",(config['run']['run_id'],)).fetchall()
    if len(receipts) != 1:
        raise RuntimeError('Expected exactly one durable patch receipt')
    saved=json.loads(receipts[0][0])
    if saved['path']!=source.name or (workspace/saved['retained_original']).read_bytes()!=original:
        raise RuntimeError('Missing retained original')
    return dict(pin=pin,tools=approved,original_sha256=original_sha,result_sha256=result_sha,
                matched_preview=True,matching_retained_original=True,reopened_without_replay=True)
