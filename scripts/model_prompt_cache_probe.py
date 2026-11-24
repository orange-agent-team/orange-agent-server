"""Measure the pinned engine's own cache-use counters on consecutive private requests."""
import http.client
import json
import os
from pathlib import Path
import secrets
import socket

def main():
    settings=json.loads(Path(os.environ["ORANGE_LOCAL_MODEL_CONFIG"]).read_bytes())
    prefix="A shared runtime serves separate requests. " * 25
    counters={}
    for family in ("chat/completions","responses"):
        observed=[]
        for _ in range(2):
            tag="R"+secrets.token_hex(3)
            message=prefix+"Reply with "+tag+" only."
            payload={"model":"qwen-local-test-v1","stream":False,"temperature":0,"cache_prompt":False}
            if family=="responses":
                payload.update(input=message,max_output_tokens=16,store=False)
            else:
                payload.update(messages=[{"role":"user","content":message}],max_tokens=16)
            client=http.client.HTTPConnection("localhost",timeout=45)
            try:
                client.sock=socket.socket(socket.AF_UNIX)
                client.sock.settimeout(45);client.sock.connect(settings["socket_path"])
                client.request("POST","/v1/"+family,json.dumps(payload),{"Content-Type":"application/json","Authorization":"Bearer "+settings["api_key"]})
                resp=client.getresponse();data=json.loads(resp.read(1024*1024))
                if resp.status!=200:raise RuntimeError("Pinned CPU rejected "+family)
                usage=data["usage"]
                inp,out,details=("prompt_tokens","completion_tokens","prompt_tokens_details") if family=="chat/completions" else ("input_tokens","output_tokens","input_tokens_details")
                if usage[inp]<50 or usage[out]<=0 or usage[details]["cached_tokens"]!=0:
                    raise ValueError("Private request reused cached tokens: "+str(usage))
                observed.append({"input":usage[inp],"output":usage[out],"cached":usage[details]["cached_tokens"]})
            finally:client.close()
        counters[family]=observed
    print(json.dumps({"same_owned_process":True,"measured_prompt_cache":counters}),flush=True)

if __name__=="__main__":main()
