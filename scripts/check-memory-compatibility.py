#!/usr/bin/env python3
"""Actual fixed memory data -> current schema28, with no HTTP/model inference."""
import argparse, hashlib, json, os, re, shutil, sqlite3, subprocess, tempfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser();parser.add_argument('--legacy-runtime',required=True,type=Path);parser.add_argument('--continuity',action='store_true');args=parser.parse_args()
legacy=args.legacy_runtime.resolve();manifest_path=legacy/'release-manifest.json';manifest=json.loads(manifest_path.read_text())
known={
 '0de3f2ccc33b70bebfc8858ee7f5ab41ea4b9174':(26,'283ed52467292292e240b7f3220dee0196a7ab262d425ad3508d0ddd1ca175ef'),
 'fb69ea634f67e6ec6d624f9efb566ce89759c39d':(27,'58e9600c22bdd56c50df3c8687437591931790318df80c2c270fb275c3940712')}
pinned=known.get(manifest.get('origin_commit'))
if pinned is None or hashlib.sha256(manifest_path.read_bytes()).hexdigest()!=pinned[1]:raise SystemExit('Exact fixed supported Runtime required')
legacy_schema=pinned[0]
if args.continuity and legacy_schema!=27:raise SystemExit('Human-continuity check requires the actual schema27 producer')

actual=set()
for directory,folders,names in os.walk(legacy,followlinks=False):
    if any((Path(directory)/name).is_symlink() for name in folders):raise SystemExit('Linked source directory')
    for name in names:
        p=Path(directory)/name;rel=p.relative_to(legacy).as_posix()
        if p.is_symlink() or not p.is_file():raise SystemExit('Irregular source')
        actual.add(rel)
        if rel!='release-manifest.json' and hashlib.sha256(p.read_bytes()).hexdigest()!=manifest['content_sha256'].get(rel):raise SystemExit('Fixed source mismatch')
assert actual==set(manifest['content_sha256'])|{'release-manifest.json'}
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip());env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
assert subprocess.check_output([str(cargo.parent/'rustc'),'--version'],env=env,text=True).startswith('rustc 1.97.1 ')
def package_ids(text):
    records=set()
    for block in text.split('[[package]]')[1:]:
        fields=[]
        for key in ['name','version','source','checksum']:
            match=re.search(r'^'+key+r' = ("[^"\n]*")$',block,re.M);fields.append(json.loads(match.group(1)) if match else None)
        records.add(tuple(fields))
    return records
stage=ROOT/'.local/memory-compat';stage.mkdir(parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(dir=stage,prefix='actual-') as directory:
    folder=Path(directory);binaries=[]
    for label,source in [('old',legacy),('new',ROOT)]:
        project=folder/label;(project/'src').mkdir(parents=True)
        (project/'src/main.rs').write_bytes((ROOT/'scripts/fixtures'/('memory-continuity-compat.rs' if args.continuity else 'memory-v1-compat.rs')).read_bytes())
        dependencies={'orange-agent-runtime':source/'crates/agent-runtime','orange-protocol':source/'crates/protocol'}
        text='[workspace]\nresolver="2"\n[package]\nname="orange-memory-compat"\nversion="0.1.0"\nedition="2024"\n[dependencies]\n'
        for name,path in dependencies.items():text+=name+'={path='+json.dumps(str(path))+'}\n'
        text+='tokio={version="=1.54.1",features=["full"]}\nserde_json="=1.0.154"\n'
        (project/'Cargo.toml').write_text(text);(project/'Cargo.lock').write_bytes((source/'Cargo.lock').read_bytes())
        target=ROOT/('.local/schema'+str(legacy_schema)+'-compat-target' if label=='old' else 'target');child=env.copy();child['CARGO_TARGET_DIR']=str(target)
        subprocess.run([str(cargo),'metadata','--offline','--format-version','1'],cwd=project,env=child,stdout=subprocess.DEVNULL,check=True,timeout=60)
        allowed=package_ids((source/'Cargo.lock').read_text())|{('orange-memory-compat','0.1.0',None,None)}
        assert package_ids((project/'Cargo.lock').read_text())<=allowed,'Harness introduced an unlocked dependency'
        subprocess.run([str(cargo),'build','--locked','--offline'],cwd=project,env=child,check=True,timeout=240);binaries.append(target/'debug/orange-memory-compat')
    case=folder/'state';(case/'private').mkdir(parents=True,mode=0o700);(case/'workspace').mkdir(mode=0o700)
    subprocess.run([str(binaries[0]),'seed',str(case)],env=env,check=True,timeout=60)
    dbpath=case/'private/journal.sqlite'
    def snapshot():
        with sqlite3.connect(dbpath) as db:
            return {'schema':db.execute('PRAGMA user_version').fetchone()[0], 'operations':db.execute('SELECT operation_id,request_json,argument_digest,tool_version,result_digest,result_json,model_result,context_applied FROM operations ORDER BY operation_id').fetchall(), 'sources':db.execute('SELECT * FROM runtime_memory_sources ORDER BY operation_id').fetchall(), 'catalogs':db.execute('SELECT invocation,revision,document_digest,document FROM runtime_tool_catalogs ORDER BY invocation').fetchall()}
    before=snapshot();assert before['schema']==legacy_schema and len(before['sources'])==1 and len(before['operations'])==(2 if args.continuity else 1)
    subprocess.run([str(binaries[1]),'resume',str(case)],env=env,check=True,timeout=60)
    after=snapshot();assert after['schema']==28;assert before['operations']==after['operations'];assert before['sources']==after['sources'];assert all(row in after['catalogs'] for row in before['catalogs'])
    with sqlite3.connect(dbpath) as db:assert db.execute("SELECT read_version FROM runtime_memory_protocol WHERE run_id='run'").fetchone()==(1,)
    def durable_bytes():return {p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in [dbpath,dbpath.with_name(dbpath.name+'-wal')] if p.exists()}
    digest=durable_bytes();old=subprocess.run([str(binaries[0]),'inspect',str(case)],env=env,capture_output=True,text=True,timeout=30);assert old.returncode!=0,'old reader must refuse';assert 'UnsupportedVersion' in old.stderr or 'unsupported journal' in old.stderr.lower(),old.stderr;assert durable_bytes()==digest,'old reader mutated newer data'
print(json.dumps({'result':'PASS','scenario':('actual-schema27-human-confirmation-to28' if args.continuity else 'actual-schema'+str(legacy_schema)+'-memory-v1-to28'),'old_binary_seeded_and_approved_read':True,'old_tool_snapshots_and_results_preserved':True,'protocol_retained_v1':True,'read_replays':0,'known_source_required_after_confirmation':args.continuity,'raw_store_bypass_rejected':args.continuity,'withdrawal_after_migration_rejected':args.continuity,'old_binary_rejected_without_mutation':True,'transport':'explicit scripted model; no actual inference'},indent=2))
