"""Versioned, deterministic files shared by macOS and Linux platform archives."""
import gzip
import io
from pathlib import Path
import subprocess
import tarfile

def service_files(repository,commit,readme,extra=()):
    def git(*args):return subprocess.check_output(["git",*args],cwd=repository)
    def names(folder):return git("ls-tree","-r","--name-only",commit,"--",folder).decode().splitlines()
    files={}
    def put(source,name=None):
        name=name or source
        if name in files:raise ValueError("Duplicate release path: "+name)
        files[name]=git("show",commit+":"+source)
    put("distribution/platform/verify.py","verify.py")
    put("distribution/platform/verify-candidate-signature.py","verify-candidate-signature.py")
    put("distribution/platform/reconcile-wallets.py","reconcile-wallets.py")
    put("scripts/sign-model-certification.py","scripts/sign-model-certification.py")
    put("docs/model-registry-drafts.md")
    put("contracts/registry/user-rpc-authority.v1.json")
    put("docs/engineering/user-rpc-boundaries.md")
    put("docs/engineering/versioned-artifact-storage.md")
    put("distribution/platform/recover-private-artifacts.py","recover-private-artifacts.py")
    put("distribution/platform/sql/replay-artifact-redactions.sql","sql/replay-artifact-redactions.sql")
    put("distribution/platform/sql/reconcile-wallets.sql","sql/reconcile-wallets.sql")
    if readme:put(readme,"README.md")
    for source in names("distribution/platform/config"):put(source,"config/"+Path(source).name)
    for folder in ("contracts/openapi","contracts/json-schema","contracts/proto","services/control-plane/migrations"):
        for source in names(folder):put(source,"migrations/"+Path(source).name if folder.endswith("migrations") else source)
    for source in ("Cargo.lock","rust-toolchain.toml","crates/protocol/generation.lock.json","config/worker-development.example.json","config/trusted-meter.example.json",
                   "config/worker-isolation-release.v1.json","config/connector-releases.v1.json","docs/worker-development-host.md","docs/development-cloud-host.md",
                   "docs/cloud-isolation-development.md","docs/audio-artifact-preflight.md","docs/local-http-metrics.md","docs/engineering/api-configuration-diagnostics.md","docs/engineering/job-state-revisions.md","docs/engineering/owned-job-events.md",
                   "docs/events-queue-implementation.md","docs/worker/nvidia-inventory.md","docs/worker/isolation-report.md","docs/worker/device-enrollment-request.md","docs/worker/device-certificate-install.md","docs/worker/enrollment-authorization.md","docs/worker/development-device-issuer.md","docs/engineering/device-issuer-directory.md","docs/security/credential-purposes.md",
                   "docs/security/data-lifecycle.md","docs/security/quarantined-private-root-recovery.md","docs/security/data-lifecycle.json","docs/engineering/identity-deadlines.md","docs/database/transaction-keys.json","docs/database/transactions-and-retention.md",*extra):put(source)
    return files

def write_archive(path,files,prefix="orange-platform"):
    if prefix not in ("orange-platform","orange-market-report","orange-knowledge","orange-model-manifest"):raise ValueError("Unknown distribution prefix")
    with path.open("xb") as raw,gzip.GzipFile(fileobj=raw,mode="wb",filename="",mtime=0) as gz,tarfile.open(fileobj=gz,mode="w") as tar:
        for name,data in sorted(files.items()):
            item=tarfile.TarInfo(prefix+"/"+name)
            item.size=len(data);item.mode=0o755 if name.startswith("bin/") else 0o644
            tar.addfile(item,io.BytesIO(data))
