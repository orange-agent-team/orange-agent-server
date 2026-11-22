#!/usr/bin/env python3
"""Package the operator's offline report CLI separately from the six service executables."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
from platform_payload import write_archive

ROOT=Path(__file__).resolve().parents[1]
def git(*args):return subprocess.check_output(['git',*args],cwd=ROOT)
def sha(value):return hashlib.sha256(value).hexdigest()
def main():
    if git('status','--porcelain'):raise ValueError('Clean committed source required')
    head=git('rev-parse','HEAD').decode().strip()
    subprocess.run([sys.executable,ROOT/'scripts/build-embedding-release.py','--profile','market-report','--commit',head],cwd=ROOT,check=True)
    dest=ROOT/'.local/market-report-distributions';dest.mkdir(exist_ok=True)
    native=ROOT/'.local/market-report-release'/head;build=json.loads((native/'native-build.json').read_bytes())
    name='orange-market-report'
    binary=(native/name).read_bytes()
    if build['source_commit']!=head or build['target']!='aarch64-apple-darwin' or build['builds']!=[{name:sha(binary)}]*2 or not build['reproducible']:
        raise ValueError('Missing exact native build pair')
    files={'bin/'+name:binary}
    for path in [*git('ls-files','docs/market-metrics').decode().splitlines(),'contracts/json-schema/market-metrics.snapshot.v1.json','Cargo.lock','rust-toolchain.toml',
                 'services/control-plane/migrations/0059_purchase_lots.sql','services/control-plane/migrations/0064_merchant_statements.sql','services/control-plane/migrations/0066_provider_earned_credits.sql']:
        files[path]=git('show',head+':'+path)
    files['README.md']='在本目录运行 bin/orange-market-report docs/market-metrics/sample.snapshot.json。输入为已授权的去标识化快照，详细指标见 docs/market-metrics/README.md。\n'.encode()
    files['manifest.json']=(json.dumps({'source':head,'target':build['target'],'sha256':{p:sha(data) for p,data in files.items()}},sort_keys=True,indent=2)+'\n').encode()
    with tempfile.TemporaryDirectory(prefix='market-release-',dir=dest) as tmp:
        work=Path(tmp);a=work/'a.tgz';b=work/'b.tgz';write_archive(a,files,prefix=name);write_archive(b,files,prefix=name)
        data=a.read_bytes();digest=sha(data)
        if data!=b.read_bytes():raise ValueError('Archive differs between assemblies')
        with tarfile.open(a) as tar:tar.extractall(work/'unpacked',filter='data')
        extracted=work/'unpacked'/name
        for path,content in files.items():
            if sha((extracted/path).read_bytes())!=sha(content):raise ValueError('Unpacked content mismatch')
        out=subprocess.check_output([extracted/'bin'/name,extracted/'docs/market-metrics/sample.snapshot.json'],cwd=extracted)
        if json.loads(out)!=json.loads(files['docs/market-metrics/sample.report.json']):raise ValueError('Archived CLI produced a different report')
        path=dest/('orange-market-report-macos-arm64-'+digest[:16]+'.tar.gz')
        if path.exists():
            if path.is_symlink() or path.read_bytes()!=data:raise ValueError('Refusing to replace conflicting report archive')
        else:a.rename(path)
        receipt={'source':head,'target':build['target'],'archive':path.name,'sha256':digest,'bytes':len(data),'independent_builds':build['builds'],
                 'identical_archives':2,'sample_report_from_extracted_binary':True,'scope':'macOS ARM64 offline development reporting utility; no live payment or merchant certificate'}
        print(json.dumps(receipt),flush=True)

if __name__=='__main__':main()
