#!/usr/bin/env python3
"""Verify each PRD-006 deliverable through the actual read-only report executable."""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT=Path(__file__).resolve().parents[1]
DOC=ROOT/'docs/market-metrics'
SECTIONS=('supply','economics','events','reconciliation')
def load(name):return json.loads((DOC/name).read_bytes())
def require(value,reason):
    if not value:raise ValueError(reason)
def item(s,t):
    return next(e for e in s['events'] if e['fact']['type']==t)
def ident(value):return hashlib.sha256(value.encode()).hexdigest()
def add(s,kind,at,**fields):
    s['events'].append({'receipt':ident('additional-event-'+str(len(s['events']))),'at':at,'fact':{'type':kind,**fields}})
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--section',choices=SECTIONS,required=True)
    args=parser.parse_args()
    target=Path(os.environ.get('CARGO_TARGET_DIR',ROOT/'target')).resolve()
    rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo=Path(subprocess.check_output([rustup,'which','--toolchain','1.97.1','cargo'],cwd=ROOT,text=True).strip())
    env=dict(os.environ,CARGO_NET_OFFLINE='true',CARGO_TARGET_DIR=str(target),PATH=str(cargo.parent)+os.pathsep+os.environ.get('PATH',''))
    subprocess.run([cargo,'build','--locked','--offline','-p','orange-control-plane','--bin','orange-market-report'],cwd=ROOT,env=env,check=True)
    original=load('sample.snapshot.json');sources=load('sample.sources.json');expected=load('sample.report.json')
    with tempfile.TemporaryDirectory(prefix='orange-market-sample-') as folder:
        exe=Path(folder)/'market-report';shutil.copy2(target/'debug/orange-market-report',exe)
        def invoke(s,reject=False):
            f=Path(folder)/'input.json';f.write_text(json.dumps(s));done=subprocess.run([exe,f],capture_output=True,text=True,timeout=15)
            if reject:
                require(done.returncode!=0 and not done.stdout.strip() and 'PRIVATE_' not in done.stderr,'Invalid projection accepted or content leaked')
                return
            require(done.returncode==0,'Report projection failed: '+done.stderr[:160])
            return json.loads(done.stdout)
        actual=invoke(original)
        require(actual==expected,'Committed report differs from current implementation')
        if args.section=='supply':
            a=actual['supply'];require([a[k] for k in ('registered_nodes','effective_nodes','completed_nodes')]==[3,2,1],'Node populations conflated')
            require(a['utilization']=={'numerator':'1320','denominator':'4800'} and a['paid_outside_eligible_seconds']=='0','Incorrect eligible interval union')
            part=copy.deepcopy(original);part['from_unix']+=900;part['until_unix']=part['from_unix']+600
            require(invoke(part)['supply']['utilization']=={'numerator':'0','denominator':'600'},'Pause clipped incorrectly')
            p=copy.deepcopy(original);p['events']=[e for e in p['events'] if e['fact']['type']!='capacity']
            a=invoke(p)['supply'];require(a['utilization'] is None and a['paid_outside_eligible_seconds']=='1320','Unreconciled paid time concealed')
            p=copy.deepcopy(original);item(p,'attempt')['fact']['node']=ident('not-registered');invoke(p,True)
        elif args.section=='economics':
            usd=actual['currency_totals'][0];require((usd['net_receipts_minor'],usd['attributed_service_minor'],usd['contribution_minor'])==('650','170','77'),'Cash or service value incorrect')
            require(usd['contribution_rate']=={'numerator':'77','denominator':'170'},'Margin ratio incorrect')
            p=copy.deepcopy(original);p['cost_coverage'][0]['complete'].remove('egress')
            require(invoke(p)['currency_totals'][0]['contribution_minor'] is None,'Partial cost treated as total cost')
            p=copy.deepcopy(original);q=item(p,'settlement')['fact'];q['lots'][0]['units']=str(int(q['purchased'])-1)
            v=invoke(p);require(v['microcredits']['unattributed_purchased']=='1' and v['currency_totals'][0]['contribution_minor'] is None,'Invented a price for untracked credits')
            p=copy.deepcopy(original);at=p['until_unix']-1;order=item(p,'refund')['fact']['order'];dispute=ident('dispute')
            add(p,'chargeback',at,order=order,dispute=dispute,minor='10');add(p,'chargeback_recovery',at,dispute=dispute,minor='3')
            v=invoke(p)['currency_totals'][0];require(v['net_receipts_minor']=='643' and v['open_dispute_minor_as_of']=='7' and v['contribution_minor'] is None,'Dispute cash or margin incorrect')
            add(p,'chargeback_recovery',at,dispute=dispute,minor='8');invoke(p,True)
            p=copy.deepcopy(original);add(p,'expense',at,currency='eur',category='storage',minor='15')
            by={row['currency']:row for row in invoke(p)['currency_totals']};require(by['usd']==usd and by['eur']['known_cost_minor']=='15' and by['eur']['contribution_minor'] is None,'Mixed currencies')
            p=copy.deepcopy(original);q=item(p,'settlement');p['until_unix']+=86400;p['as_of_unix']=p['until_unix']
            add(p,'earning_released',q['at']+86400,job=q['fact']['job'],units=q['fact']['provider_pending'])
            require(invoke(p)['microcredits']['provider_released']=='800','Review release not accounted independently')
            p['events'][-1]['at']-=1;invoke(p,True)
        elif args.section=='events':
            schema=json.loads((ROOT/'contracts/json-schema/market-metrics.snapshot.v1.json').read_bytes())
            branches=schema['$defs']['fact']['oneOf']
            for branch in branches:
                require(branch['additionalProperties'] is False and branch['required'],'Open event projection')
            require({b['properties']['type']['const'] for b in branches}=={'node_registered','capacity','attempt','job_submitted','job_finished','settlement','earning_released','purchase','refund','expense','chargeback','chargeback_recovery'},'Event schema missing a source')
            p=copy.deepcopy(original);p['events'].reverse();p['events'].append(copy.deepcopy(p['events'][0]))
            require(invoke(p)==actual,'Out-of-order delivery or replay changed totals')
            for key in ('prompt','audio','email','PRIVATE_key_material'):
                p=copy.deepcopy(original);item(p,'job_submitted')['fact'][key]='PRIVATE_credential';invoke(p,True)
            p=copy.deepcopy(original);item(p,'purchase')['fact']['payer']='PRIVATE_raw_user';invoke(p,True)
            p=copy.deepcopy(original);extra=copy.deepcopy(p['events'][0]);extra['at']+=1;p['events'].append(extra);invoke(p,True)
            p=copy.deepcopy(original);extra=copy.deepcopy(item(p,'settlement'));extra['receipt']=ident('different receipt');p['events'].append(extra);invoke(p,True)
        else:
            by_type=lambda kind:[e['fact'] for e in original['events'] if e['fact']['type']==kind]
            purchases={r['order']:r for r in by_type('purchase')};refunds={r['refund']:r for r in by_type('refund')}
            merchant=sources['merchant'];seen=set();net=0
            for r in merchant:
                identifier=r['reference'];require(identifier not in seen,'Duplicate merchant source');seen.add(identifier)
                fact=(purchases if r['flow']=='purchase' else refunds)[identifier]
                when=next(e['at'] for e in original['events'] if e['fact']==fact)
                require(r['at_unix']==when and r['in_window']==(original['from_unix']<=when<original['until_unix']) and r['original_order']==fact['order'],'Original payment reference or period changed')
                parent=fact if r['flow']=='purchase' else purchases[fact['order']]
                sign=1 if r['flow']=='purchase' else -1
                require((parent['currency'],parent['payer'],int(fact['minor'])*sign,int(fact['units'])*sign)==(r['currency'],r['payer'],int(r['minor']),int(r['purchased_units'])),'Merchant line does not reconcile')
                if r['in_window']:net+=int(r['minor'])
            require(seen==set(purchases)|set(refunds) and net==int(actual['currency_totals'][0]['net_receipts_minor']),'Unmatched merchant event')
            settlements={a['job']:a for a in by_type('settlement')};outcomes={a['job']:a['outcome'] for a in by_type('job_finished')};checked=set()
            for row in sources['ledger']:
                job=row['job'];require(job not in checked,'Duplicate ledger accounting row');checked.add(job)
                values=[int(row[k]) for k in ('gift','earned','purchased','provider_pending','platform_fee')]
                require(sum(values)==0,'Unbalanced ledger line')
                require([int(row['returned'][k])-int(row['reservation'][k]) for k in ('gift','earned','purchased')]==values[:3],'Reserve and released buckets do not reconcile')
                if outcomes[job]=='failed':require(not any(values) and job not in settlements,'Failure charged')
                else:
                    s=settlements[job]
                    require(values==[-int(s['gift']),-int(s['earned']),-int(s['purchased']),int(s['provider_pending']),int(s['platform_fee'])],'Settlement buckets changed')
            require(checked==set(outcomes),'Unmatched closed job')
            cost=actual['currency_totals'][0];require(cost['costs_minor']==sources['cost_vouchers_minor'],'Unmatched cost voucher')
            require(actual['demand']['repurchase_rate']=={'numerator':'1','denominator':'2'} and actual['demand']['failed']==1 and actual['microcredits']['gift_used']=='200','Original scenarios missing')
    print('PASS original PRD-006 '+args.section+': deterministic Rust report, scoped development inputs and explicit missing observations',flush=True)

if __name__=='__main__':main()
