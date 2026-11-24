#!/usr/bin/env python3.12
"""Resolve the Debian runtime closure with the pinned target's APT and a verified Bookworm index."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import io
import json
import lzma
from pathlib import Path
import re
import subprocess
import tarfile
import urllib.request
import uuid
from linux_platform_inputs import local_image_id

ROOT=Path(__file__).resolve().parents[1];ENV=ROOT/"backends/environments/sdxl-linux-cuda";CACHE=ROOT/".local/debian-inputs"
ORIGIN="https://deb.debian.org/debian/"


def fetch(path,limit):
    with urllib.request.urlopen(ORIGIN+path,timeout=60) as response:
        raw=response.read(limit+1)
        if response.status!=200 or len(raw)>limit:raise ValueError("Unexpected Debian response")
        return raw


def main():
    CACHE.mkdir(exist_ok=True,parents=True)
    pin=json.loads((ENV/"base-image.lock.json").read_bytes());d=["docker","--context","colima-orange-agent"]
    base=local_image_id(d,{"image":pin["repository"]+"@"+pin["manifest_digest"],"config_digest":pin["config_digest"],"platform":"linux/amd64"})
    if not base:raise ValueError("Missing pinned interpreter image")
    release=fetch("dists/bookworm/InRelease",1<<20)
    signed=subprocess.check_output([*d,"run","--rm","-i","--pull=never","--platform","linux/amd64","--network","none","--read-only",
                  "--cap-drop","ALL","--entrypoint","/usr/bin/gpgv",base,"--keyring","/usr/share/keyrings/debian-archive-keyring.gpg","--output","-","-"],input=release)
    text=signed.decode()
    if not re.search(r"^Codename: bookworm$",text,re.M):raise ValueError("Wrong release")
    name="main/binary-amd64/Packages.xz"
    section=text.split("\nSHA256:\n",1)[1].split("\nSHA512:",1)[0]
    sha,size,_=next(line.split() for line in section.splitlines() if line.split() and line.split()[-1]==name)
    size=int(size)
    if not 0<size<24*1024*1024:raise ValueError("Unbounded package index")
    compressed=fetch("dists/bookworm/"+name,size)
    if hashlib.sha256(compressed).hexdigest()!=sha:raise ValueError("Debian package index hash differs")
    index=lzma.decompress(compressed)
    nonce=uuid.uuid4().hex
    cid=subprocess.check_output([*d,"create","--label","orange.owned="+nonce,"--pull=never","--platform","linux/amd64","--network","none","--cap-drop","ALL",
                                 "--security-opt","no-new-privileges","--entrypoint","apt-get",base,"-s","--no-install-recommends","install","libgl1"],text=True).strip()
    try:
        with subprocess.Popen([*d,"cp","-",cid+":/var/lib/apt/lists"],stdin=subprocess.PIPE) as child:
            with tarfile.open(fileobj=child.stdin,mode="w|") as archive:
                for filename,data in [("deb.debian.org_debian_dists_bookworm_InRelease",release),("deb.debian.org_debian_dists_bookworm_main_binary-amd64_Packages",index)]:
                    item=tarfile.TarInfo(filename);item.mode=0o644;item.size=len(data);archive.addfile(item,io.BytesIO(data))
            child.stdin.close()
            if child.wait():raise RuntimeError("Could not stage signed index")
        plan=subprocess.check_output([*d,"start","-a",cid],text=True,timeout=120)
        if subprocess.check_output([*d,"inspect","--format","{{.State.ExitCode}}",cid],text=True).strip()!="0" or "Remv " in plan:raise ValueError("Unexpected APT solution: "+plan[-600:])
    finally:
        if subprocess.check_output([*d,"inspect","--format",'{{index .Config.Labels "orange.owned"}}',cid],text=True).strip()==nonce:
            subprocess.run([*d,"rm","-f",cid],check=True,stdout=subprocess.DEVNULL)
    targets={name:version for name,version in re.findall(r"^Inst (\S+)(?: \[[^\]]+\])? \((\S+)",plan,re.M)}
    if not {"libgl1","libglvnd0","libglx0","libglx-mesa0"}<=targets.keys():raise ValueError("APT solution lacked GL dependencies: "+plan[:600])
    rows=[]
    for para in index.decode().split("\n\n"):
        p=dict(row.split(": ",1) for row in para.splitlines() if not row.startswith(" ") and ": " in row)
        if targets.get(p.get("Package"))!=p.get("Version") or p.get("Architecture") not in ("amd64","all"):continue
        if not re.fullmatch(r"pool/main/[A-Za-z0-9+._~/-]+\.deb",p["Filename"]) or ".." in p["Filename"]:raise ValueError("Unexpected Debian file")
        rows.append({"name":p["Package"],"version":p["Version"],"architecture":p["Architecture"],"bytes":int(p["Size"]),"path":p["Filename"],
                     "sha256":p["SHA256"],"depends":p.get("Depends","")})
    if len(rows)!=len(targets) or {r["name"] for r in rows}!=targets.keys():raise ValueError("Ambiguous APT solution")
    def download(r):
        out=CACHE/Path(r["path"]).name
        if not out.is_file() or hashlib.sha256(out.read_bytes()).hexdigest()!=r["sha256"]:
            data=fetch(r["path"],r["bytes"])
            if len(data)!=r["bytes"] or hashlib.sha256(data).hexdigest()!=r["sha256"]:raise ValueError("Invalid "+r["name"])
            out.write_bytes(data)
    with ThreadPoolExecutor(max_workers=3) as pool:list(pool.map(download,rows))
    doc={"origin":ORIGIN,"codename":"bookworm","component":"main","architecture":"amd64","base_digest":pin["manifest_digest"],
         "inrelease_sha256":hashlib.sha256(release).hexdigest(),"packages_index_sha256":sha,"date":next(l[6:] for l in text.splitlines() if l.startswith("Date: ")),
         "packages":sorted(rows,key=lambda v:v["name"])}
    (ENV/"debian-libs.lock.json").write_text(json.dumps(doc,indent=2)+"\n");(CACHE/"InRelease").write_bytes(release)
    print(json.dumps({"signed_index":sha,"packages":[r["name"] for r in rows],"bytes":sum(r["bytes"] for r in rows)}),flush=True)

if __name__=="__main__":main()
