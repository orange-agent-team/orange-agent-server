"""Exercise the local model and local knowledge process together, using only private Unix IPC."""
from http.client import HTTPConnection
import hashlib
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time

def digest(path):
    with path.open("rb") as f:return hashlib.file_digest(f,"sha256").hexdigest()

def exercise(repo,runtime,binary,peer_type,*,persistent=False):
    lock=json.loads((repo/"config/local-model.lock.json").read_bytes())
    build=json.loads((runtime/"build.json").read_bytes());engine=runtime/"build/bin/llama-server"
    model=runtime/(lock["model"]["sha256"]+".gguf")
    if (model.stat().st_size!=lock["model"]["bytes"] or digest(model)!=lock["model"]["sha256"]
        or build["backend_commit"]!=lock["backend"]["commit"] or build["gpu_enabled"] or digest(engine)!=build["executable_sha256"]):
        raise ValueError("Verified locked CPU model and engine required")
    with tempfile.TemporaryDirectory(prefix="oa-kq-",dir="/private/tmp" if os.uname().sysname=="Darwin" else None) as tmp:
        base=Path(tmp);private=base/"p";workspace=base/"w";private.mkdir(mode=0o700);workspace.mkdir(mode=0o700)
        uds=private/"engine.sock";key=secrets.token_hex(32);keyfile=private/"key";keyfile.write_text(key);keyfile.chmod(0o600)
        code="ATLAS-"+secrets.token_hex(3).upper()
        (workspace/"dispatch.txt").write_text("The harbor dispatch verification code is "+code+".")
        (workspace/"calendar.txt").write_text("The maintenance appointment is on Wednesday.")
        (workspace/"unselected.txt").write_text("The private dispatch verification code is FORBIDDEN.")
        class Unix(HTTPConnection):
            def connect(self):
                self.sock=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);self.sock.settimeout(self.timeout);self.sock.connect(str(uds))
        def request(method,path,data=None):
            c=Unix("localhost",timeout=90)
            try:
                c.request(method,path,body=None if data is None else json.dumps(data),headers={"Authorization":"Bearer "+key,"Content-Type":"application/json"})
                r=c.getresponse();raw=r.read(1024*1024+1)
                if r.status!=200 or len(raw)>1024*1024:raise ValueError(f"Model endpoint returned {r.status}")
                return json.loads(raw)
            finally:c.close()
        alias="knowledge-cpu-qualification"
        args=[engine,"--model",model,"--host",uds,"--api-key-file",keyfile,"--alias",alias,"--threads","2","--ctx-size","4096","--parallel","1",
              "--n-gpu-layers","0","--jinja","--no-webui","--no-warmup","--no-context-shift"]
        with (private/"engine.log").open("wb") as out, subprocess.Popen(args,stdin=subprocess.DEVNULL,stdout=out,stderr=out) as proc:
            client=None
            try:
                end=time.monotonic()+90
                while True:
                    if proc.poll() is not None:raise ValueError("CPU model exited before readiness")
                    try:
                        if request("GET","/health").get("status")=="ok":break
                    except (OSError,ValueError):pass
                    if time.monotonic()>=end:raise ValueError("CPU model readiness timeout")
                    time.sleep(.15)
                client=peer_type(binary,"development-owner",workspace,private,["dispatch.txt","calendar.txt"],persistent=persistent);client.start()
                definitions={t["name"]:t for t in client.call("tools/list")["result"]["tools"]}
                messages=[{"role":"system","content":"Use only the provided local tools and retrieved document text to answer. The collection is manuals."}]
                operations=[]
                def run(name,prompt,approve,selected_citation=None):
                    messages.append({"role":"user","content":prompt})
                    t=definitions[name]
                    schema=json.loads(json.dumps(t["inputSchema"]))
                    if selected_citation is not None:
                        # Limit the model's next offer to the exact hit already selected by the test owner,
                        # without filling in or repairing its returned tool arguments.
                        for field,value in selected_citation.items():
                            schema["properties"][field]["enum"]=[value]
                        schema["properties"]={k:schema["properties"][k] for k in selected_citation}
                    tool={"type":"function","function":{"name":name,"description":t["description"],"parameters":schema}}
                    result=request("POST","/v1/chat/completions",{"model":alias,"messages":messages,"temperature":0,"seed":42,"max_tokens":256,"tools":[tool],
                                   "tool_choice":{"type":"function","function":{"name":name}}})
                    choice,=result["choices"];msg=choice["message"];call,=msg["tool_calls"]
                    if choice["finish_reason"]!="tool_calls" or call["function"]["name"]!=name or not call["id"]:raise ValueError("No complete expected model call")
                    args=json.loads(call["function"]["arguments"])
                    if not approve(args):
                        # All names and hashes here are owned test data. Record shape, never document contents.
                        shape={k:len(v) if isinstance(v,str) else type(v).__name__ for k,v in args.items()}
                        raise ValueError(f"Test owner rejected actual model arguments for {name}: {shape}")
                    received=client.call("tools/call",{"name":name,"arguments":args})["result"]
                    if received["isError"]:raise ValueError("Approved model tool request failed: "+name)
                    messages.extend([{"role":"assistant","content":msg.get("content") or "","tool_calls":[call]},
                                     {"role":"tool","tool_call_id":call["id"],"content":received["content"][0]["text"]}])
                    operations.append(call["id"])
                    return received["structuredContent"]["data"]
                hits=run("knowledge_search","Search collection manuals for the words harbor dispatch. Set query to harbor dispatch and limit to 1.",
                         lambda a:a.get("collection_id")=="manuals" and a.get("query","").lower()=="harbor dispatch" and a.get("limit")==1)["hits"]
                hit,=hits
                if hit["path"]!="dispatch.txt":raise ValueError("Wrong source chosen")
                if persistent:
                    client.close();client=None
                    client=peer_type(binary,"development-owner",workspace,private,["calendar.txt","dispatch.txt"],persistent=True)
                    client.start()
                selected={"collection_id":"manuals","hit_id":hit["hit_id"],"expected_sha256":hit["source_sha256"]}
                body=run("knowledge_read","Read the entire returned hit. Pass collection_id manuals, hit_id "+hit["hit_id"]+" and expected_sha256 "+hit["source_sha256"]+". Do not add any other arguments.",
                         lambda args:args==selected,selected)
                if code not in body["text"]:raise ValueError("Wrong retrieved source")
                messages.append({"role":"user","content":"What is the verification code? Reply with the exact code from the document."})
                answer=request("POST","/v1/chat/completions",{"model":alias,"messages":messages,"temperature":0,"seed":42,"max_tokens":64})
                final=answer["choices"][0]
                if final["finish_reason"]!="stop" or code not in final["message"]["content"] or "FORBIDDEN" in final["message"]["content"]:raise ValueError("Answer is not grounded in the selected passage")
                return {"backend_commit":build["backend_commit"],"model_sha256":lock["model"]["sha256"],"model_calls":3,"reopened_after_actual_model_search":persistent,"approved_tool_calls":len(operations),
                        "answer_matched_unpublished_nonce":True,"source_sha256":hit["source_sha256"],"approval":"explicit per-operation test owner; read offer pinned to the selected live citation","transport":"private Unix socket and stdio"}
            finally:
                try:
                    if client:client.close()
                finally:
                    if proc.poll() is None:
                        proc.terminate()
                        try:proc.wait(timeout=10)
                        except subprocess.TimeoutExpired:proc.kill();proc.wait()
