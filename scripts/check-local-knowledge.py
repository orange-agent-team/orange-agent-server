"""Exercise the selected-file knowledge host over actual stdio with a measured bilingual retrieval corpus."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT=Path(__file__).resolve().parents[1]

def require(value,message):
    if not value:raise ValueError(message)

class Peer:
    def __init__(self,binary,owner,root,private,files,profile=None):
        self.process=subprocess.Popen([binary,*(["--protocol",profile] if profile else []),owner,"manuals",root,private,*files],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
        self.serial=0
    def call(self,method,params=None,notice=False):
        msg={"jsonrpc":"2.0","method":method}
        if params is not None:msg["params"]=params
        if not notice:self.serial+=1;msg["id"]=self.serial
        self.process.stdin.write(json.dumps(msg,ensure_ascii=False)+"\n");self.process.stdin.flush()
        if notice:return None
        reply=json.loads(self.process.stdout.readline());require(reply["id"]==self.serial and reply["jsonrpc"]=="2.0","Wrong response identity")
        return reply
    def start(self):
        require("error" in self.call("tools/list"),"Uninitialized listing")
        args={"protocolVersion":"2025-11-25","clientInfo":{"name":"qualification","version":"1"},"capabilities":{}}
        require("error" in self.call("initialize",dict(args,protocolVersion="2025-06-18")),"Accepted unsupported version")
        require(self.call("initialize",args)["result"]["protocolVersion"]=="2025-11-25","No version agreement")
        self.call("notifications/initialized",notice=True)
        require([t["name"] for t in self.call("tools/list")["result"]["tools"]]==["knowledge_search","knowledge_read"],"Incorrect advertised surface")
    def tool(self,name,**args):
        return self.call("tools/call",{"name":name,"arguments":{"collection_id":"manuals",**args}})
    def search(self,query,**kw):
        return self.tool("knowledge_search",query=query,**kw)["result"]["structuredContent"]["data"]
    def read(self,hit,**kw):
        return self.tool("knowledge_read",hit_id=hit["hit_id"],expected_sha256=hit["source_sha256"],**kw)["result"]
    def close(self):
        self.process.stdin.close()
        try:require(self.process.wait(timeout=10)==0,"Knowledge host failed")
        finally:
            if self.process.poll() is None:self.process.kill();self.process.wait()
            self.process.stdout.close()

def main():
    p=argparse.ArgumentParser();p.add_argument("--binary",type=Path);p.add_argument("--cpu-runtime",type=Path);a=p.parse_args()
    binary=a.binary
    if not binary:
        rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
        cargo=Path(subprocess.check_output([rustup,"which","cargo"],cwd=ROOT,text=True).strip())
        env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
        subprocess.run([cargo,"build","--offline","--locked","-p","orange-agent-runtime","--bin","orange-knowledge"],cwd=ROOT,env=env,check=True)
        meta=json.loads(subprocess.check_output([cargo,"metadata","--offline","--locked","--no-deps","--format-version","1"],cwd=ROOT,env=env))
        binary=Path(meta["target_directory"])/"debug/orange-knowledge"
    with tempfile.TemporaryDirectory(prefix="orange-knowledge-") as tmp:
        root=Path(tmp)/"workspace";private=Path(tmp)/"private";root.mkdir();private.mkdir()
        corpus={"shipping.txt":"The cobalt parcel leaves on Thursday.\n蓝色包裹周四发货。",
                "maintenance.txt":"Renew the lantern certificate in September.\n灯塔证书九月续期。",
                "training.txt":"The mountain workshop uses room twenty.\n山地培训在二十号教室。",
                "guide.txt":"The library returns are due on Tuesday.\n图书应在周二归还。"}
        hidden="UNSELECTED-SENTINEL";(private/"private.txt").write_text(hidden);(root/"unselected.txt").write_text(hidden)
        (root/"symlink.txt").symlink_to(private/"private.txt");os.link(private/"private.txt",root/"hardlink.txt")
        for path,body in corpus.items():(root/path).write_text(body)
        allowed=[*corpus,"selected-copy.txt","symlink.txt","hardlink.txt"]
        peer=Peer(binary,"fixture-owner",root,private,allowed)
        try:
            peer.start()
            peer.process.stdin.write('{"jsonrpc":"2.0","id":599,"method":"tools/list","method":"ping"}\n');peer.process.stdin.flush()
            ambiguous=json.loads(peer.process.stdout.readline());require(ambiguous.get("id") is None and ambiguous["error"]["code"]==-32600,"Duplicate method accepted")
            require(peer.tool("knowledge_search",query="harbor",owner="other")["error"]["code"]==-32602,"Untrusted owner field accepted")
            cases=[("cobalt parcel","shipping.txt"),("周四发货","shipping.txt"),("lantern certificate","maintenance.txt"),("灯塔证书","maintenance.txt"),
                   ("mountain workshop","training.txt"),("培训","training.txt"),("library returns","guide.txt"),("图书","guide.txt")]
            for query,path in cases:
                hits=peer.search(query)["hits"];require(hits and hits[0]["path"]==path,"Wrong top citation for "+query)
                hit=hits[0];require(hit["source_sha256"]==hashlib.sha256(corpus[path].encode()).hexdigest(),"Wrong source version")
                body=peer.read(hit)["structuredContent"]["data"];require(body["text"]==corpus[path],"Unverified citation text")
            # Use JSON-RPC notifications on the real pipe. The accepted progress token binds both reports.
            ident=900;token="progress-9"
            msg={"jsonrpc":"2.0","id":ident,"method":"tools/call","params":{"name":"knowledge_search",
                 "_meta":{"progressToken":token},"arguments":{"collection_id":"manuals","query":"cobalt"}}}
            peer.process.stdin.write(json.dumps(msg)+"\n");peer.process.stdin.flush()
            observed=[json.loads(peer.process.stdout.readline()) for _ in range(3)]
            require([v["params"]["progress"] for v in observed[:2]]==[0,1] and all(v["params"]["progressToken"]==token for v in observed[:2])
                and observed[2]["id"]==ident and not observed[2]["result"]["isError"],"Missing progress or call identity")
            omitted=peer.search(hidden)
            require(not omitted["hits"] and omitted["unavailable_sources"]==3,"Unselected or linked content disclosed, or omissions not reported")
            require(peer.tool("knowledge_search",query=hidden,path="unselected.txt")["result"]["isError"],"Unselected path filter accepted")
            hit=peer.search("cobalt")["hits"][0]
            mismatched=peer.tool("knowledge_read",hit_id=hit["hit_id"],expected_sha256="0"*64)["result"]
            require(mismatched["isError"] and "structuredContent" not in mismatched,"Mismatched source digest returned data")
            require(peer.read(hit,start_byte=0,end_byte=3)["structuredContent"]["data"]["text"]=="The","Wrong byte range")
            require(peer.read(hit,start_byte=2,end_byte=1)["isError"],"Reversed byte range")
            other=Peer(binary,"other-owner",root,private,allowed)
            try:
                other.start();require(other.read(hit)["isError"],"Foreign host redeemed a citation")
            finally:other.close()
            path=root/"shipping.txt";replacement=root/"replacement";replacement.write_text(corpus["shipping.txt"]);os.replace(replacement,path)
            require(peer.read(hit)["isError"],"Byte-identical replacement revived old citation")
            new=peer.search("cobalt")["hits"][0];require(new["hit_id"]!=hit["hit_id"],"Replacement kept old identity")
            path.write_text("A changed timetable.");require(peer.read(new)["isError"],"Edited citation returned stale text")
            require(not peer.search("cobalt")["hits"],"Outdated term survived incremental refresh")
            current=peer.search("changed")["hits"][0];path.unlink();require(peer.read(current)["isError"],"Deleted file returned")
            require(not peer.search("changed")["hits"],"Deleted file indexed")
            # Duplicate content shares one ranked entry, but each authorized path has its own citation.
            copy=root/"selected-copy.txt";copy.write_text(corpus["maintenance.txt"])
            both=peer.search("lantern")["hits"];require(len(both)==1 and both[0]["path"]=="maintenance.txt","Duplicate version duplicated ranking")
            alias=peer.search("lantern",path=copy.name)["hits"][0];require(alias["hit_id"]!=both[0]["hit_id"] and alias["source_sha256"]==both[0]["source_sha256"],"Lost path-specific provenance")
            (root/"maintenance.txt").rename(root/"not-selected-renamed.txt")
            require(peer.read(both[0])["isError"],"Moved source still readable")
            require(peer.read(alias)["structuredContent"]["data"]["text"]==corpus["maintenance.txt"],"Deleting one source revoked another source's identical version")
            new_hits=peer.search("lantern")["hits"];require(len(new_hits)==1 and new_hits[0]["path"]==copy.name,"Unselected rename followed, or surviving source lost")
            (root/"maintenance.txt").write_text(corpus["maintenance.txt"])
            require(not peer.search("cobalt OR lantern")["hits"],"Query words interpreted as raw FTS syntax")
        finally:peer.close()
        modern=Peer(binary,"owner",root,private,allowed,"2026-07-28")
        try:
            meta={"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}
            require(modern.call("server/discover")["error"]["code"]==-32602,"Discovery accepted missing per-request metadata")
            mismatch=modern.call("tools/list",{"_meta":{**meta,"io.modelcontextprotocol/protocolVersion":"future"}})
            require(mismatch["error"]["code"]==-32022 and mismatch["error"]["data"]=={"requested":"future","supported":["2026-07-28"]},"Wrong version error")
            discovery=modern.call("server/discover",{"_meta":meta})["result"];require(discovery["resultType"]=="complete" and discovery["supportedVersions"]==["2026-07-28"],"Discovery failed")
            require(modern.call("tools/list")["error"]["code"]==-32602,"Discovery silently opened a session")
            definition=modern.call("tools/list",{"_meta":meta})["result"];require(definition["cacheScope"]=="private" and definition["ttlMs"]==0,"Collection-specific catalog cached globally")
            read=lambda name,args:modern.call("tools/call",{"_meta":meta,"name":name,"arguments":{"collection_id":"manuals",**args}})["result"]
            h=read("knowledge_search",{"query":"lantern"})["structuredContent"]["data"]["hits"][0]
            text=read("knowledge_read",{"hit_id":h["hit_id"],"expected_sha256":h["source_sha256"]})
            require(text["resultType"]=="complete" and text["structuredContent"]["data"]["text"]==corpus["maintenance.txt"],"Direct modern calls did not read the authorized version")
        finally:modern.close()
        # A deliberately cold, larger index keeps the original call outstanding when the cancellation arrives.
        for n in range(16):(root/f"bulk{n}.txt").write_text(("cobalt private content "+str(n)+"\n")*6000)
        cancelled=Peer(binary,"owner",root,private,[f"bulk{n}.txt" for n in range(16)])
        try:
            cancelled.start()
            st=cancelled.process.stdin
            batch=[{"jsonrpc":"2.0","id":800,"method":"tools/call","params":{"name":"knowledge_search","_meta":{"progressToken":700},
                    "arguments":{"collection_id":"manuals","query":"cobalt"}}},
                   {"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":800}},
                   {"jsonrpc":"2.0","id":801,"method":"ping"}]
            st.write("".join(json.dumps(x)+"\n" for x in batch));st.flush()
            started=json.loads(cancelled.process.stdout.readline());pong=json.loads(cancelled.process.stdout.readline())
            require(started.get("params",{}).get("progress")==0 and pong.get("id")==801,"Cancellation did not preserve responsive transport")
            require(len(cancelled.search("cobalt",limit=1)["hits"])==1,"Cancellation leaked a reply or disabled the whole grant")
        finally:cancelled.close()
        print(json.dumps({"top1_hits":len(cases),"queries":len(cases),"correct_source_versions":len(cases),"languages":["en","zh"],"stdio_profiles":["2025-11-25","2026-07-28"],
                          "selected_documents":len(corpus),"foreign_citation_rejected":True,"deleted_and_replaced_citations_rejected":True,
                          "unselected_and_linked_content_returned":0,"cancelled_call_returned_content":False,"progress_token_checked":True,"storage":"in-memory full-text index","external_model_calls":0}))
        print("PASS live local knowledge stdio, explicit file authority, bilingual retrieval and version-bound citations")
    if a.cpu_runtime:
        from knowledge_model_acceptance import exercise
        result=exercise(ROOT,a.cpu_runtime.resolve(),binary,Peer)
        print(json.dumps(result),flush=True)
        print("PASS actual CPU model proposed both knowledge tools and returned the code from the authorized source")
if __name__=="__main__":main()
