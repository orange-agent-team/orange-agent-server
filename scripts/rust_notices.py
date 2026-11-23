"""Verified Cargo registry license texts, checksums and upstream supplements for platform packages."""
import hashlib
import json
import tomllib

def sha(data):return hashlib.sha256(data).hexdigest()

def notices(files,vendor,lockfile,source_blob):
    origins=json.loads(source_blob("distribution/notices/rust/provenance.json"))
    inventory=[]
    locked={(entry["name"],entry["version"]):entry.get("checksum") for entry in tomllib.loads(lockfile.decode())["package"] if entry.get("source","").startswith("registry+")}
    for (name,version),checksum in sorted(locked.items()):
        directory=vendor/(name+"-"+version)
        meta=tomllib.loads((directory/"Cargo.toml").read_text())["package"]
        checked=json.loads((directory/".cargo-checksum.json").read_bytes())
        if checked["package"]!=checksum:raise ValueError("Registry provenance mismatch: "+name)
        candidates={p for p in directory.iterdir() if p.is_file() and p.name.upper().startswith(("LICENSE","LICENCE","COPYING","NOTICE"))}
        if meta.get("license-file"):candidates.add(directory/meta["license-file"])
        for extra in directory.iterdir():
            if extra.is_dir() and not extra.is_symlink() and extra.name.lower() in ("licenses","licences"):
                candidates.update(p for p in extra.rglob("*") if p.is_file())
        authors=directory/"AUTHORS"
        if authors.is_file() and authors.read_bytes().startswith(b"LICENSE:"):
            candidates.add(authors)
        included=[]
        for p in sorted(candidates):
            if p.is_symlink() or not p.is_file() or not p.resolve().is_relative_to(directory.resolve()):raise ValueError("Unsafe license source: "+name)
            rel=p.relative_to(directory).as_posix();out=f"licenses/cargo/{name}-{version}/{rel}"
            data=p.read_bytes()
            if sha(data)!=checked["files"].get(rel):raise ValueError("Cargo source notice changed: "+str(p))
            files[out]=data;included.append(out)
        provenance=origins.get(name+"-"+version)
        if provenance:
            if not provenance["local"].startswith("distribution/notices/rust/"):raise ValueError("Unexpected upstream notice path")
            source=source_blob(provenance["local"])
            vcs=json.loads((directory/".cargo_vcs_info.json").read_bytes())
            if sha(source)!=provenance["sha256"] or vcs["git"]["sha1"]!=provenance["commit"]:raise ValueError("Upstream notice mismatch")
            if hashlib.sha1(b"blob "+str(len(source)).encode()+b"\0"+source).hexdigest()!=provenance["blob"]:raise ValueError("Git notice object mismatch")
            out="licenses/upstream/"+provenance["local"].removeprefix("distribution/notices/rust/")
            files[out]=source;included.append(out)
        inventory.append({"name":name,"version":version,"registry_checksum":checksum,"declared_license":meta.get("license"),"notice_files":included,"additional_upstream_source":provenance})
    result={"scope":"All registry sources pinned in Cargo.lock, including optional and other-target packages.","packages":inventory,
            "without_packaged_notice":[r["name"]+" "+r["version"] for r in inventory if not r["notice_files"]]}
    files["licenses/inventory.json"]=(json.dumps(result,sort_keys=True,indent=2)+"\n").encode()
    return len(inventory),len(result["without_packaged_notice"])
