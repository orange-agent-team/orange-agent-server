#!/usr/bin/env python3
"""Verify original PRD-002 workflow contracts and regenerate the corresponding human-readable diagrams."""
import argparse
import json
from pathlib import Path
import re
root=Path(__file__).resolve().parents[1]
directory=root/'docs/product-journeys'
flows=json.loads((directory/'flows.json').read_bytes())
apis={}
worker=(root/'contracts/proto/worker.proto').read_text()
def resolve(spec):
    family,method,path=spec
    if family=='worker':
        if method!='RPC' or not re.search(r'\brpc\s+'+re.escape(path)+r'\s*\(',worker):raise ValueError('Unknown Worker method '+path)
        return None
    if family not in apis:apis[family]=json.loads((root/'contracts/openapi'/(family+'.v1.json')).read_bytes())
    return apis[family]['paths'][path][method.lower()]
def display(ref):
    family,method,path=ref
    return 'WorkerGateway.'+path if method=='RPC' else method+' '+path
def render():
    lines=['# 六条核心用户流程','','这些流程区分个人、组织、提供方设备和平台操作员。示例中的现金使用测试商户协议，微积分使用开发账本。业务是否完成由原编号对应的服务端状态确认。','']
    for f in flows:
        lines.extend(['## '+f['title'],'',f['actor']+'发起这项流程。'+f['preconditions'],'',
             '入口为 `'+display(f['entry'])+'`。完成后通过 `'+display(f['terminal'])+'` 核对 '+', '.join('`'+key+'`' for key in f['terminal_fields'])+'。','',
             '```mermaid','flowchart TD'])
        for n,step in enumerate(f['steps']):
            lines.append('  S'+str(n)+'["'+step['label']+'"]')
            if n:lines.append('  S'+str(n-1)+' --> S'+str(n))
        lines.extend(['```','','| 操作 | 对应接口 |','|---|---|'])
        lines.extend('| '+step['label']+' | `'+display(step['interface'])+'` |' for step in f['steps'])
        lines.extend(['','| 退出情况 | 可观察状态 | 资金影响 | 恢复入口 |','|---|---|---|---|'])
        lines.extend('| '+kind+' | '+v['state']+' | '+v['money']+' | '+v['recovery']+' |' for kind,v in f['exits'].items())
        lines.append('')
    lines.extend(['## 演练','','完整的本机演练使用同一组个人账户、组织钱包、两个分别登记的 CPU 节点和两个独立的测试财务身份。服务端通过真实用户接口履约，支付与退款的商户端仅为本地协议替身。独立的模型进程对公开录音进行实际推理。使用 `python3 scripts/check-transcription-input-grants.py --six-journeys-only --web-repo ../orange-agent-web` 运行；浏览器购买端与管理端的故障分支通过 `scripts/check-test-checkout.py` 单独验证。',''])
    return '\n'.join(lines)
parser=argparse.ArgumentParser()
parser.add_argument('--step',choices=['entries','normal','exits','all'],default='all')
parser.add_argument('--render',action='store_true')
args=parser.parse_args()
if [f['id'] for f in flows]!=['consume','supply','team','privacy','purchase','refund']:raise SystemExit('Unexpected product coverage')
for f in flows:
    if args.step in ('entries','all'):
        resolve(f['entry']);operation=resolve(f['terminal'])
        if not f['actor'] or not f['preconditions'] or f['terminal'][1]!='GET':raise ValueError('Unqueryable terminal')
        schema=operation['responses']['200']['content']['application/json']['schema']
        resolved=apis[f['terminal'][0]]['components']['schemas'][schema['$ref'].split('/')[-1]]
        if not set(f['terminal_fields'])<=set(resolved['required']):raise ValueError(f['id']+' lacks a declared terminal field')
    if args.step in ('normal','all'):
        if len(f['steps'])<5 or f['steps'][-1]['interface']!=f['terminal']:raise ValueError('Incomplete path '+f['id'])
        for s in f['steps']:resolve(s['interface'])
    if args.step in ('exits','all'):
        if set(f['exits'])!={'取消','无权限','不可用','结果未知'}:raise ValueError('Missing exit '+f['id'])
        for v in f['exits'].values():
            if any(not v.get(k) for k in ('state','money','recovery')):raise ValueError('Missing effect or recovery')
doc=directory/'README.md';expected=render()
if args.render:doc.write_text(expected)
elif doc.read_text()!=expected:raise SystemExit('Published diagrams differ from reviewed workflows')
print('PASS six original product specifications, step '+args.step+'. Runtime proof belongs to the separately executed six-journey case.')
