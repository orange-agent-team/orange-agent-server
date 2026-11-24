import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {mkdtemp, mkdir, writeFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {Client} from '@modelcontextprotocol/client';
import {StdioClientTransport} from '@modelcontextprotocol/client/stdio';

if (process.version !== 'v22.23.2' || process.argv.length !== 3) {
  throw Error('Use pinned Node 22.23.2 and supply the knowledge executable');
}
const binary=resolve(process.argv[2]);
const root=await mkdtemp(join(tmpdir(),'orange-sdk-knowledge-'));
const work=join(root,'docs'), privateDir=join(root,'private');
try {
  await mkdir(work);await mkdir(privateDir,{mode:0o700});
  await writeFile(join(work,'bulk.txt'),'heavy data words. '.repeat(14000));
  const content='Atlas launches from the east pier. 登船地点是东码头。';
  const path=join(work,'travel.txt');
  const expected=createHash('sha256').update(content).digest('hex');
  for (const version of ['2025-11-25','2026-07-28']) {
    await writeFile(path,content);
    const client=new Client({name:'orange-sdk-qualification',version:'0.1.0'},{capabilities:{},versionNegotiation:{mode:version==='2026-07-28'?{pin:version}:'legacy'}});
    const transport=new StdioClientTransport({command:binary,args:['--protocol',version,'reader','library',work,privateDir,'travel.txt','bulk.txt'],stderr:'inherit'});
    try {
      await client.connect(transport);
      assert.equal(client.getNegotiatedProtocolVersion(),version);
      const list=await client.listTools();
      assert.deepEqual(list.tools.map(t=>t.name),['knowledge_search','knowledge_read']);
      const cancel=new AbortController();
      await assert.rejects(client.callTool(
        {name:'knowledge_search',arguments:{collection_id:'library',path:'bulk.txt',query:'heavy'}},
        {signal:cancel.signal,onprogress:()=>cancel.abort(Error('Test owner cancelled'))}));
      assert.ok(cancel.signal.aborted);
      const search=await client.callTool({name:'knowledge_search',arguments:{collection_id:'library',path:'travel.txt',query:'东码头'}});
      assert.equal(search.isError,false);
      const [hit]=search.structuredContent.data.hits;
      assert.equal(hit.source_sha256,expected);
      const read={name:'knowledge_read',arguments:{collection_id:'library',hit_id:hit.hit_id,expected_sha256:expected}};
      const valid=await client.callTool(read);
      assert.equal(valid.isError,false);
      assert.equal(valid.structuredContent.data.text,content);
      await rm(path);
      const denied=await client.callTool(read);
      assert.equal(denied.isError,true);
      assert.ok(!JSON.stringify(denied).includes('Atlas'));
      console.log('PASS official MCP TypeScript client '+version+' discovery, cancel, tool calls and deleted citation');
    } finally {await client.close();}
  }
  for (const version of ['2025-11-25','2026-07-28']) {
    const collection='saved-'+version;
    await writeFile(path,content);
    async function scoped(operation) {
      const client=new Client({name:'orange-knowledge-restart',version:'0.1.0'},
        {capabilities:{},versionNegotiation:{mode:version==='2026-07-28'?{pin:version}:'legacy'}});
      const transport=new StdioClientTransport({command:binary,args:['--protocol',version,'--persist','reader',collection,work,privateDir,'travel.txt']});
      try {
        await client.connect(transport);
        assert.equal(client.getNegotiatedProtocolVersion(),version);
        return await operation(client);
      } finally {await client.close();}
    }
    const hit=await scoped(async(client)=>{
      const r=await client.callTool({name:'knowledge_search',arguments:{collection_id:collection,query:'Atlas'}});
      assert.equal(r.isError,false);
      return r.structuredContent.data.hits[0];
    });
    await scoped(async(client)=>{
      const r=await client.callTool({name:'knowledge_read',arguments:{collection_id:collection,hit_id:hit.hit_id,expected_sha256:hit.source_sha256}});
      assert.equal(r.isError,false);assert.equal(r.structuredContent.data.text,content);
    });
    const withdrawal=JSON.parse(execFileSync(binary,['--withdraw-saved','reader',collection,privateDir],{encoding:'utf8'}));
    assert.equal(withdrawal.state,'withdrawn');
    await assert.rejects(scoped(async()=>true));
    console.log('PASS official MCP '+version+' persistent citation after reconnect and permanent withdrawal');
  }
} finally {await rm(root,{recursive:true,force:true});}
