"""Exercise installed release executables on a separately provided pinned E5 model cache."""
import argparse
import json
import math
import os
from pathlib import Path
import selectors
import subprocess
import tempfile

root = Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument("venv", type=Path)
parser.add_argument("model_dir", type=Path)
args = parser.parse_args()
python = args.venv.absolute() / "bin/python"
model = args.model_dir.resolve()
with tempfile.TemporaryDirectory(prefix="oe-dist-", dir="/tmp") as runtime:
    host = subprocess.Popen([root / "bin/orange-embedding-host", python, model],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                            env={"TMPDIR": runtime, "PATH": "/usr/bin:/bin"})
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(host.stdout, selectors.EVENT_READ)
            if not selector.select(20):
                raise RuntimeError("Released host did not listen")
        session = json.loads(host.stdout.readline())
        socket = Path(session["socket"])
        if not socket.resolve().is_relative_to(Path(runtime).resolve()):
            raise ValueError("Invalid private socket")
        texts = ["query: What is the capital of Japan?", "passage: The capital of Japan is Tokyo.",
                 "passage: Baking bread requires yeast."]
        frame = {"socket": str(socket), "token":session["token"], "texts":texts}
        completed = subprocess.run([root / "bin/local-embeddings"],input=json.dumps(frame), text=True,
                                   capture_output=True, check=True, timeout=100, env={"PATH":"/usr/bin:/bin"})
        result = json.loads(completed.stdout)
        rows = result["data"]
        if (result["model"] != "multilingual-e5-small-onnx-cpu-v1" or len(rows)!=3
            or result["usage"] != {"prompt_tokens":34,"total_tokens":34}):
            raise ValueError("Unexpected profile, batch size or usage")
        vectors = []
        for i, row in enumerate(rows):
            vector = row["embedding"]
            if row["index"]!=i or len(vector)!=384 or not math.isclose(sum(x*x for x in vector),1,abs_tol=0.0001):
                raise ValueError("Invalid ordered vector")
            vectors.append(vector)
        def score(index):
            return sum(a*b for a,b in zip(vectors[0],vectors[index]))
        if score(1)<=score(2):
            raise ValueError("Unexpected semantic retrieval order")
    finally:
        host.stdin.close()
        try:
            host.wait(timeout=100)
        except subprocess.TimeoutExpired:
            host.kill()
            host.wait()
            raise
    if host.returncode!=0 or socket.exists():
        raise ValueError("Released model host failed to clean up")
    print(json.dumps({"model":result["model"],"vectors":3,"dimensions":384,"tokens":34,
                      "related_cosine":score(1),"other_cosine":score(2),"clean_shutdown":True}))
