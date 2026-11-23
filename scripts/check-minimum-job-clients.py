"""Run the original and current user Web clients against the same captured development HTTP responses."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile

MINIMUM="c0ffde5b93de4a3310f0113d841c6c0216e2ab6c"

TEST="""import { expect, test } from 'vitest';
import { readFileSync } from 'node:fs';
import { ComputeClient } from '../src/api/compute-client';
const fixture=JSON.parse(readFileSync(new URL('./compatibility-response.json',import.meta.url),'utf8'));
const session={user_id:'test-user',session_id:'test-session',personal_owner_id:fixture.owner,
               csrf_token:'unused-for-read',expires_at:'2099-01-01T00:00:00Z'};
function client(record) {
    return new ComputeClient(async (path,init)=>{
        expect(path).toBe('/api/v1/owners/'+fixture.owner+'/jobs/'+fixture.job);
        expect(init.method ?? 'GET').toBe('GET');
        return new Response(JSON.stringify(record.body),{status:record.status,headers:record.headers});
    });
}
test('the same typed job and monetary fields survive additive HTTP revision metadata',async()=>{
    for (const record of [fixture.before,fixture.after]) {
        expect(await client(record).job(session,fixture.job)).toEqual(record.body);
    }
    const exact={...fixture.before,body:{...fixture.before.body,reserved:'9007199254740993'}};
    expect((await client(exact).job(session,fixture.job)).reserved).toBe('9007199254740993');
});
test('unknown authority and mismatched job identity fail before data is returned',async()=>{
    const record=fixture.before;
    await expect(client({...record,body:{...record.body,allow_spend:true}}).job(session,fixture.job)).rejects.toThrow();
    await expect(client({...record,body:{...record.body,job_id:'different-job'}}).job(session,fixture.job)).rejects.toThrow();
    await expect(client(fixture.denied).job(session,fixture.job)).rejects.toMatchObject({status:401});
});
"""
CURRENT_TEST="""test('current client reads the revision and still handles its omission',async()=>{
    const r=fixture.before;
    expect((await client(r).jobSnapshot(session,fixture.job)).revision).toBe(r.headers['x-orange-job-version']);
    const headers={...r.headers};delete headers['x-orange-job-version'];
    expect(await client({...r,headers}).jobSnapshot(session,fixture.job)).toEqual({job:r.body,revision:undefined});
});
"""


def check(repo, current, sample):
    if not re.fullmatch(r"[0-9a-f]{40}",current) or current==MINIMUM:
        raise ValueError("An independently pinned current Web revision is required")
    node=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
    npm=node.parent.parent/"lib/node_modules/npm/bin/npm-cli.js"
    env=dict(os.environ,PATH=str(node.parent)+os.pathsep+os.environ["PATH"],
             NPM_CONFIG_CACHE=str(repo/".local/npm-cache"),NPM_CONFIG_OFFLINE="true",NPM_CONFIG_AUDIT="false",
             NPM_CONFIG_FUND="false")
    if subprocess.check_output([node,"--version"],text=True).strip()!="v22.23.2":
        raise ValueError("Pinned Node required for both clients")
    results=[]
    for revision in [MINIMUM,current]:
        with tempfile.TemporaryDirectory(prefix="orange-minimum-reader-") as directory, tempfile.TemporaryFile() as src:
            root=Path(directory)
            subprocess.run(["git","archive",revision],cwd=repo,check=True,stdout=src)
            src.seek(0)
            with tarfile.open(fileobj=src) as archive:archive.extractall(root,filter="data")
            app=root/"apps/web"
            subprocess.run([node,npm,"ci","--ignore-scripts","--offline","--no-audit","--no-fund"],cwd=app,env=env,check=True)
            name="tests/pinned-client.test.ts"
            (app/name).write_text(TEST+(CURRENT_TEST if revision==current else ""))
            (app/"tests/compatibility-response.json").write_text(json.dumps(sample))
            run=subprocess.run([node,app/"node_modules/vitest/vitest.mjs","run",name,"--maxWorkers=1"],cwd=app,
                               env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            print(run.stdout,flush=True)
            run.check_returncode()
            expected=3 if revision==current else 2
            if f"{expected} passed" not in run.stdout:
                raise ValueError("Missing named client cases")
            results.append({"source":revision,"passed":expected})
    return results


if __name__=="__main__":
    cli=argparse.ArgumentParser()
    cli.add_argument("--web-repo",type=Path,required=True)
    cli.add_argument("--sample",type=Path,default=Path(__file__).resolve().parents[1]/"contracts/fixtures/http-job-v1/job-history.json")
    args=cli.parse_args()
    corpus=json.loads(args.sample.read_bytes())
    if corpus.get("synthetic_development_fixtures") is not True or corpus["readers"][0]["source"]!=MINIMUM:
        raise ValueError("Expected the approved sanitized development Job corpus")
    print(json.dumps(check(args.web_repo.resolve(),corpus["readers"][1]["source"],corpus["sample"])),flush=True)
