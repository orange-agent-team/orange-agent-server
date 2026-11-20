"""Private guest entry point. Stdin carries the pinned runtime archive and static probe."""
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import selectors
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import uuid

def sha(path):
    h=hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda:f.read(1024*1024),b''):
            h.update(chunk)
    return h.hexdigest()

def copy_member(tar,member,dest,mode):
    source=tar.extractfile(member)
    if source is None:
        raise ValueError('Missing archive member')
    with source,dest.open('xb') as output:
        shutil.copyfileobj(source,output)
    dest.chmod(mode)

if os.geteuid()!=0 or platform.machine()!='aarch64' or platform.system()!='Linux':
    raise SystemExit('Dedicated root-owned ARM64 Linux qualification required')
with tempfile.TemporaryDirectory(prefix='orange-gvisor-') as temp:
    root=Path(temp)
    root.chmod(0o711)  # unprivileged runtime helpers can traverse, but cannot list this directory
    bounds={'archive':77679372,'probe':16*1024*1024,'profile':8192}
    seen=set()
    with tarfile.open(fileobj=sys.stdin.buffer,mode='r|') as packet:
        for m in packet:
            if m.name not in bounds or m.name in seen or not m.isfile() or not 0<m.size<=bounds[m.name]:
                raise ValueError('Unexpected transfer member')
            seen.add(m.name)
            copy_member(packet,m,root/m.name,0o600)
    if seen!=set(bounds):
        raise ValueError('Incomplete qualification packet')
    profile=json.loads((root/'profile').read_text())
    archive=root/'archive'
    if sha(archive)!=profile['sha256'] or archive.stat().st_size!=profile['size_bytes']:
        raise ValueError('Runtime archive failed guest digest check')
    tools=root/'tools'
    tools.mkdir(mode=0o755)
    total=0
    with tarfile.open(archive,'r:bz2') as tar:
        for m in tar:
            rel=PurePosixPath(m.name)
            if rel.is_absolute() or '..' in rel.parts or not rel.parts or len(rel.parts)>2:
                raise ValueError('Unexpected runtime layout')
            if rel.parts[0] not in ('runsc','containerd-shim-runsc-v1','gvisor-bin'):
                raise ValueError('Unexpected runtime tool')
            dest=tools.joinpath(*rel.parts)
            if m.isdir():
                dest.mkdir(mode=0o755,exist_ok=True)
            elif m.isfile() and m.size>0:
                total+=m.size
                if total>1024**3:
                    raise ValueError('Unexpected expansion of runtime package')
                copy_member(tar,m,dest,0o755)
            else:
                raise ValueError('Unsupported runtime archive entry')
    binary=tools/'runsc'
    version=subprocess.check_output([binary,'--version'],text=True).strip()
    if profile['release'] not in version:
        raise ValueError('Unrecognized runsc release: '+version)
    bundle=root/'bundle'
    rootfs=bundle/'rootfs'
    rootfs.mkdir(parents=True)
    for dirname in ['proc','dev','workspace']:
        (rootfs/dirname).mkdir()
    shutil.copyfile(root/'probe',rootfs/'probe')
    (rootfs/'probe').chmod(0o555)
    state=root/'state';state.mkdir(mode=0o700)
    marker=root/'guest-private-marker';marker.write_text('outside container')
    instance='oa-qual-'+uuid.uuid4().hex
    cgroup=Path('/sys/fs/cgroup')/instance
    if cgroup.exists():
        raise ValueError('Test cgroup already exists')
    with socket.socket() as sentinel:
        sentinel.bind(('127.0.0.1',0));sentinel.listen();sentinel.setblocking(False)
        process={'terminal':False,'user':{'uid':65534,'gid':65534},
                 'args':['/probe',str(sentinel.getsockname()[1]),str(marker)],'env':['LANG=C'],'cwd':'/workspace',
                 'capabilities':{k:[] for k in ['bounding','effective','inheritable','permitted','ambient']},'noNewPrivileges':True}
        config={'ociVersion':'1.0.2','process':process,'root':{'path':'rootfs','readonly':True},'hostname':'orange-qa',
                'mounts':[{'destination':'/proc','type':'proc','source':'proc','options':['nosuid','nodev','noexec','ro']},
                          {'destination':'/dev','type':'tmpfs','source':'tmpfs','options':['nosuid','noexec','mode=755','size=65536']},
                          {'destination':'/workspace','type':'tmpfs','source':'tmpfs','options':['nosuid','nodev','noexec','mode=1777','size='+str(profile['workspace_bytes'])]}],
                'linux':{'namespaces':[{'type':s} for s in ['pid','network','ipc','uts','mount']],
                         'cgroupsPath':'/'+instance,'resources':{'memory':{'limit':profile['memory_bytes']},'pids':{'limit':profile['pids']},
                                                              'cpu':{'quota':profile['cpu_quota'],'period':profile['cpu_period']}}}}
        (bundle/'config.json').write_text(json.dumps(config))
        args=[str(binary),'--root='+str(state),'--platform=systrap','--network=none','--gofer-network-namespace=new',
              '--sandbox-creation-cpu-policy=1x','--sandbox-lifecycle-cpu-policy=1x']
        stderr=root/'workload.stderr'
        child=None
        try:
            with stderr.open('wb') as err:
                child=subprocess.Popen([*args,'run','--bundle',str(bundle),instance],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=err)
            with selectors.DefaultSelector() as watcher:
                watcher.register(child.stdout,selectors.EVENT_READ)
                if not watcher.select(timeout=40) or child.stdout.readline()!=b'ORANGE_WORKSPACE_PROBE_READY\n':
                    raise RuntimeError('Missing running workload barrier: '+stderr.read_text(errors='replace')[-1800:])
            record=json.loads(subprocess.check_output([*args,'state',instance],timeout=10))
            if record.get('id')!=instance or record.get('status')!='running':
                raise RuntimeError('Missing running OCI state')
            limits={key:(cgroup/key).read_text().strip() for key in ['memory.max','pids.max','cpu.max']}
            expected={'memory.max':str(profile['memory_bytes']),'pids.max':str(profile['pids']),
                      'cpu.max':str(profile['cpu_quota'])+' '+str(profile['cpu_period'])}
            if limits!=expected:
                raise RuntimeError('Observed cgroup limits differ: '+str(limits))
            pid=record['pid']
            if not isinstance(pid,int) or pid<2 or ('0::/'+instance) not in Path(f'/proc/{pid}/cgroup').read_text().splitlines():
                raise RuntimeError('Running sandbox does not belong to the observed cgroup')
            output,_=child.communicate(input=b'continue\n',timeout=40)
            if child.returncode!=0 or output.strip()!=b'ORANGE_WORKSPACE_PROBE_OK':
                raise RuntimeError('Workload failed: '+str(child.returncode)+' '+stderr.read_text(errors='replace')[-1800:])
            try:
                unexpected,_=sentinel.accept()
            except BlockingIOError:
                pass
            else:
                unexpected.close()
                raise RuntimeError('Sandbox reached guest-only loopback sentinel')
        finally:
            if child is not None and child.poll() is None:
                if child.stdin and not child.stdin.closed:
                    child.stdin.close()
            listed=subprocess.check_output([*args,'list','--format=json'],timeout=15)
            if any(v.get('id')==instance for v in json.loads(listed) or []):
                subprocess.run([*args,'delete','--force',instance],check=True,timeout=30)
            if child is not None:
                child.wait(timeout=30)
        if (state/instance).exists() or cgroup.exists() or json.loads(subprocess.check_output([*args,'list','--format=json'],timeout=10)):
            raise RuntimeError('Sandbox cleanup not confirmed')
    print(json.dumps({'qualified':True,'profile':profile['profile'],'runsc':version,'archive_sha256':profile['sha256'],
                      'probe_sha256':sha(root/'probe'),'kernel':platform.release(),'platform':'systrap','network':'none',
                      'cgroup':limits,'workspace_bytes':profile['workspace_bytes'],'workload_uid':65534,'effective_capabilities':0,
                      'root_readonly':True,'sentinel_unreachable':True,'sandbox_and_cgroup_removed':True,
                      'scope':'one owned static payload in local Linux ARM64 VM; production pool, Web Agent and GPU are separate'}))
