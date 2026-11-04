#!/usr/bin/env python3
"""Verify frozen projection bytes, real tokenizer IDs and independent integer-price expectations.

No golden rewriting, model inference, bill collection, or paid support-unit certification.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT=Path(__file__).resolve().parents[1]
PRIVATE=ROOT/'.local/model-runtime'
def digest(path):
    h=hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda:stream.read(1024*1024),b''):h.update(block)
    return h.hexdigest()
def main():
    lock=json.loads((ROOT/'config/local-tokenizer.lock.json').read_text())
    build=json.loads((PRIVATE/'tokenizer-build.json').read_text())
    fixture=json.loads((ROOT/'contracts/fixtures/metering-v1.json').read_text())
    assert fixture['profile_id']==lock['profile_id']
    assert (fixture['input_price_per_1000'],fixture['output_price_per_1000'],fixture['fee_basis_points'])==('2000','6000',2000)
    binary=PRIVATE/'build/bin/llama-tokenize'
    vocabulary=PRIVATE/('llama-'+lock['backend_commit'])/lock['tokenizer_file']
    assert build['target']=='llama-tokenize' and build['backend_commit']==lock['backend_commit']
    assert digest(binary)==build['executable_sha256']
    assert digest(vocabulary)==lock['tokenizer_sha256']==fixture['tokenizer_sha256']==build['vocabulary_sha256']
    assert vocabulary.stat().st_size==lock['tokenizer_bytes']
    rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
    environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
    subprocess.run([str(cargo),'build','--bin','metering-fixtures','--locked'],cwd=ROOT,env=environment,check=True)
    clean={'PATH':'/usr/bin:/bin','LANG':'en_US.UTF-8','TMPDIR':str(PRIVATE)}
    cache={}
    def tokenize(text,special=False):
        key=(text,special)
        if key not in cache:
            command=[str(binary),'-m',str(vocabulary),'--stdin','--ids','--no-bos','--no-escape']
            if not special:command+=['--no-parse-special']
            result=subprocess.run(command,input=text,text=True,env=clean,capture_output=True,timeout=20,check=True)
            ids=json.loads(result.stdout)
            assert isinstance(ids,list) and all(isinstance(i,int) and i>=0 for i in ids)
            cache[key]=ids
        return cache[key]
    def inspect(mode,text,tools,**fields):
        result=subprocess.run([str(ROOT/'target/debug/metering-fixtures')],input=json.dumps({'mode':mode,'text':text,'tools':tools,**fields}),text=True,capture_output=True,env=clean,timeout=10)
        assert result.returncode in [0,2], 'unexpected inspection process failure'
        return result.returncode,json.loads(result.stdout)
    assert tokenize(fixture['rendered_input'],True)==fixture['input_token_ids']
    input_units=len(fixture['input_token_ids'])
    assert len(tokenize('<|im_end|>',False))>1 and len(tokenize('<|im_end|>',True))==1
    projections={};results=[]
    for case in fixture['cases']:
        text=case.get('text',''.join(case.get('text_fragments',[])))
        tools=[{'name':t['name'],'arguments_json':t.get('arguments_json',''.join(t.get('argument_fragments',[])))} for t in case['tools']]
        code,projection=inspect('project',text,tools)
        if case.get('expected_projection_error'):
            assert code==2 and projection['error']==case['expected_projection_error']
            assert case['expected_output_units'] is None  # Unknown partial usage is not fabricated as measured zero.
            units=0;text='';tools=[];delivery='uncommitted'
        else:
            assert code==0 and projection['profile_id']==fixture['profile_id']
            texts=[part['text'] for part in projection['parts']]
            assert texts==case['expected_texts']
            ids=[tokenize(part) for part in texts]
            assert ids==case['expected_token_ids']
            units=sum(map(len,ids));assert units==case['expected_output_units']
            assert units<=fixture['output_limit']
            delivery=case['delivery']
            projections[case['id']]=projection
            if case.get('equivalent_to'):assert projection==projections[case['equivalent_to']]
        code,price=inspect('price',text,tools,input_units=input_units,output_units=units,output_limit=fixture['output_limit'],delivery=delivery)
        assert code==0 and price==case['expected_price'], case['id']
        expected_charge=0 if delivery=='uncommitted' else (input_units*2000+999)//1000+(units*6000+999)//1000
        assert int(price['charge'])==expected_charge
        assert int(price['platform_fee'])==expected_charge*2000//10000
        assert int(price['provider_pending'])+int(price['platform_fee'])==expected_charge
        if case.get('old_attempt_text'):
            old_units=len(tokenize(case['old_attempt_text']));assert tokenize(case['old_attempt_text'])==case['old_attempt_token_ids']
            old_code,old_price=inspect('price',case['old_attempt_text'],[],input_units=input_units,output_units=old_units,output_limit=fixture['output_limit'],delivery='superseded')
            assert old_code==0 and old_price['charge']==case['expected_old_charge']=='0'
        results.append({'case':case['id'],'measured_output_units':case['expected_output_units'],'charge_fixture':price['charge']})
    # Nonbillable complete tools stay free until the host commits; a text-cancel label cannot bypass this.
    tool=fixture['cases'][1]
    args={'input_units':input_units,'output_units':tool['expected_output_units'],'output_limit':128}
    assert inspect('price',tool['text'],tool['tools'],delivery='uncommitted',**args)[1]['charge']=='0'
    assert inspect('price',tool['text'],tool['tools'],delivery='cancelled_persisted_text',**args)[0]==2
    assert inspect('price','x',[],input_units=input_units,output_units=129,output_limit=128,delivery='committed_success')[0]==2
    assert inspect('price','',[],input_units=input_units,output_units=0,output_limit=128,delivery='cancelled_persisted_text')[1]['charge']=='0'
    report={'profile_id':fixture['profile_id'],'tokenizer_sha256':lock['tokenizer_sha256'],'input_units':input_units,'cases':results,'scope':'actual vocabulary and fixture arithmetic only; no live inference or charges'}
    (PRIVATE/'metering-report.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,ensure_ascii=False))
    print('PASS seven fixed native-tokenizer profiles, canonical bytes, bounds, cancellation and retry-winner pricing fixtures')
if __name__=='__main__':main()
