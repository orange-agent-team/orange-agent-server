#!/usr/bin/env python3
"""Validate original PRD-005 deliverables against the named plan and current implementation paths."""
import argparse
import json
import hashlib
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]
DOC=ROOT/'docs/product-decisions'
def load(path):return json.loads(path.read_bytes())
def require(value,message):
    if not value:raise ValueError(message)
def indexed(rows):
    result={r['id']:r for r in rows}
    require(len(result)==len(rows),'Duplicate decision or dependency')
    return result

def main():
    cli=argparse.ArgumentParser();cli.add_argument('--section',choices=['facts','adr','external','changes'],required=True);cli.add_argument('--workspace',type=Path,required=True)
    args=cli.parse_args();workspace=args.workspace.resolve()
    registry=load(DOC/'register.json');facts=indexed(registry['facts']);external=indexed(load(DOC/'launch-dependencies.json')['items']);changes=indexed(load(DOC/'change-index.json')['changes'])
    require(registry['version']=='1.0.0' and registry['requirement']=='PRD-005','Missing decision version')
    if args.section=='facts':
        require({r['classification'] for r in facts.values()}=={'user_confirmed','engineering_default','requires_measurement','external_dependency'},'Invalid fact classes')
        for key,description in [('team_capacity','10 人'),('reported_device','RTX6000 96G'),('target_locations','香港')]:
            entry=facts[key];require(entry['source']=='operator_reply' and entry['classification']=='user_confirmed' and description in entry['decision'] and entry['unverified'],'Reported limits were changed')
        require(facts['fixture_rates']['classification']=='requires_measurement' and '商业费率' in (DOC/'README.md').read_text(),'Fixture fees became commercial rates')
        require(facts['repositories']['core_projects_by_repository']==[2,7,1,1],'Unknown repository allocation')
        for key in ('team_capacity','target_locations','reported_device'):
            require(facts[key]['unverified'],'Independent validation is still required')
    elif args.section=='adr':
        doc=(DOC/'architecture.md').read_text()
        require({r['adr'] for r in facts.values() if 'adr' in r}=={'control-plane','registration','credits','engines'},'Missing required architectural decision')
        for key in ('control-plane','registration','credits','engines'):
            row=next((l for l in doc.splitlines() if l.startswith('| '+key+' |')),None)
            require(row is not None and len(row.strip('|').split('|'))==4,'Missing rationale, consequence or trigger: '+key)
        import re
        for path in re.findall(r'\]\((\.\./[^)]+)\)',doc):
            target=(DOC/path).resolve();require(target.is_relative_to(ROOT) and target.exists(),'Invalid implementation source '+path)
        require((ROOT/'crates/agent-runtime/src/engine.rs').is_file() and facts['agent_engine']['source']=='approved_four_repository_plan','Unexpected Agent host selection')
    elif args.section=='external':
        require(set(external)=={'operator','merchant','domain','certificates','production_identity','target_gpu','regional_servers','enterprise_pilot'},'Incomplete external registry')
        for key,r in external.items():
            require(all(r[k].strip() for k in ('responsible','required','local_evidence')),'Missing proof specification or owner for '+key)
            if r['status']=='verified':
                proof=r.get('verification',{});path=(workspace/proof.get('file','')).resolve()
                require(proof.get('target') and proof.get('verified_at') and path.is_relative_to(workspace) and path.is_file()
                        and hashlib.sha256(path.read_bytes()).hexdigest()==proof.get('sha256'),'Missing independent target evidence for '+key)
            else:
                require(r['status'] in {'pending_external','reported_not_certified','target_not_certified'},'Unknown external status for '+key)
    else:
        entries=load(workspace/'services_plan/execution/manifest.json')['units']
        units={u['id'] for u in entries};tasks={p for u in entries for p in u.get('source_tasks',[])}
        for row in changes.values():
            require(set(row['tasks'])<=tasks and set(row['verification'])<=units,'Unknown source task or verification')
            for path in row['repository_paths']:
                target=(ROOT/path).resolve();require(target.is_relative_to(ROOT) and target.exists(),'Missing implementation target '+path)
            for figure in row['planning_figures']:
                for rel in (figure+'.md',figure+'.mmd','assets/'+figure+'.svg'):
                    require((workspace/'plan/architecture'/rel).is_file(),'Missing figure source or export '+rel)
        require(len(changes)==4 and '旧端拒绝' in (DOC/'changes.md').read_text(),'Missing compatibility review')
    print('PASS PRD-005 '+args.section+': original M0 decision record; source classification, target limits and real release qualifications stay separate.')

if __name__=='__main__':main()
