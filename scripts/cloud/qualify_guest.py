"""Private guest entry point. Stdin carries the pinned runtime archive and static probe."""
import contextlib
import fcntl
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
import struct
import tarfile
import time
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

def save_receipt(path, data):
    with tempfile.NamedTemporaryFile(dir=path.parent,delete=False) as out:
        out.write(json.dumps(data,sort_keys=True).encode())
        out.flush();os.fsync(out.fileno())
    os.replace(out.name,path)
    fd=os.open(path.parent,os.O_RDONLY)
    try:os.fsync(fd)
    finally:os.close(fd)

def remaining_mounts(directory):
    prefix=str(directory)
    return any(v.split()[4]==prefix or v.split()[4].startswith(prefix+'/')
               for v in Path('/proc/self/mountinfo').read_text().splitlines())

def finish_managed():
    if remaining_mounts(root) or cgroup.exists():
        raise RuntimeError('Workspace still mounted or charged')
    journal['phase']='runtime_gone';save_receipt(receipt_path,journal)
    if root.exists():shutil.rmtree(root)
    if root.exists():raise RuntimeError('Workspace files remain')
    journal['phase']='cleaned'
    journal['sandbox_absent']=True;journal['cgroup_absent']=True;journal['files_absent']=True
    save_receipt(receipt_path,journal)
    print(json.dumps(journal),flush=True)

def prepare_directory():
    if not managed:return tempfile.TemporaryDirectory(prefix='orange-gvisor-')
    target.mkdir(mode=0o711)
    return contextlib.nullcontext(str(target))

def bounded_packet(stream):
    if not managed:return contextlib.nullcontext(stream)
    length=struct.unpack('>Q',stream.read(8))[0]
    if not 0<length<96*1024*1024:raise ValueError('Unexpected frame length')
    staged=tempfile.TemporaryFile(dir=root)
    while length:
        chunk=stream.read(min(length,1024*1024))
        if not chunk:raise EOFError('Incomplete workspace transfer')
        staged.write(chunk);length-=len(chunk)
    staged.seek(0);return staged

def wait_for_release(stream, expires, maximum):
    # Limit permissions locally even if the supervising process stalls.
    with selectors.DefaultSelector() as watcher:
        watcher.register(stream,selectors.EVENT_READ)
        while expires>time.time():
            if not watcher.select(timeout=max(0,expires-time.time())):return
            message=stream.readline(128)
            if not message or message==b'stop\n':return
            if not message.startswith(b'renew ') or not message.endswith(b'\n'):raise ValueError('Unexpected Host command')
            next_expiry=int(message[6:-1])
            if next_expiry<=time.time() or next_expiry>min(maximum,time.time()+32):raise ValueError('Invalid lease renewal')
            expires=next_expiry

if os.geteuid()!=0 or platform.machine()!='aarch64' or platform.system()!='Linux':
    raise SystemExit('Dedicated root-owned ARM64 Linux qualification required')
managed=len(sys.argv)>1
if managed:
    if len(sys.argv)!=3 or sys.argv[1] not in ('reserve','open','recover'):
        raise SystemExit('Expected managed allocation operation and UUID')
    allocation=str(uuid.UUID(sys.argv[2]))
    if allocation!=sys.argv[2]:raise ValueError('Noncanonical allocation id')
    base=Path('/var/tmp/orange-cloud-workspaces-v1')
    base.mkdir(mode=0o711,exist_ok=True)
    if base.is_symlink() or base.stat().st_uid!=0 or base.stat().st_mode&0o022:raise ValueError('Unsafe allocation directory')
    receipt_path=base/(allocation+'.json')
    target=base/('work-'+allocation)
    instance='oa-ws-'+allocation
    cgroup=Path('/sys/fs/cgroup')/instance
    lock=os.open(base/(allocation+'.lock'),os.O_CREAT|os.O_NOFOLLOW|os.O_RDWR,0o600)
    try:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    except BlockingIOError:
        if sys.argv[1]=='recover':
            print(json.dumps({'phase':'supervisor_still_owns_allocation','allocation_id':allocation}),flush=True)
            sys.exit(0)
        raise RuntimeError('Active supervisor already owns this allocation')
    if sys.argv[1]=='recover':
        if not receipt_path.is_file():raise RuntimeError('No trusted preparation record')
        journal=json.loads(receipt_path.read_text())
        if journal.get('allocation_id')!=allocation or journal.get('sandbox_id')!=instance:raise ValueError('Conflicting allocation journal')
        root=target
        if journal['phase'] in ('starting','ready'):
            binary=root/'tools/runsc'
            if not binary.is_file() or sha(binary)!=journal['runsc_sha256']:raise RuntimeError('Incomplete runtime evidence')
            args=[str(binary),'--root='+str(root/'state'),'--gofer-network-namespace=new']
            rows=json.loads(subprocess.check_output([*args,'list','--format=json'],timeout=10)) or []
            if any(v['id']!=instance for v in rows):raise RuntimeError('Foreign sandbox in owned runtime root')
            if rows:subprocess.run([*args,'delete','--force',instance],check=True,timeout=30)
            if json.loads(subprocess.check_output([*args,'list','--format=json'],timeout=10)):raise RuntimeError('Sandbox cleanup incomplete')
        elif journal['phase']=='cleaned':
            if target.exists() or cgroup.exists():raise RuntimeError('Unexpected resources after confirmed cleanup')
            print(json.dumps(journal),flush=True);sys.exit(0)
        elif journal['phase'] not in ('registered','runtime_gone'):raise RuntimeError('Unrecognized allocation state')
        finish_managed();sys.exit(0)
    binding=json.loads(sys.stdin.buffer.readline(4097))
    if binding['allocation_id']!=allocation or not all(isinstance(binding[k],str) and 0<len(binding[k])<=128 for k in ['run_id','workspace_id','host_instance_id']):
        raise ValueError('Invalid allocation identity')
    if not isinstance(binding['epoch'],int) or binding['epoch']<1 or not isinstance(binding['input_sha256'],str) or len(binding['input_sha256'])!=64:
        raise ValueError('Invalid input binding')
    if binding['lease_until']<=time.time() or binding['lease_until']>min(binding['workspace_until'],time.time()+32):
        raise ValueError('Initial provisioning lease expired')
    identity={k:binding[k] for k in ['run_id','workspace_id','allocation_id','host_instance_id','epoch','input_sha256']}
    if receipt_path.exists():
        journal=json.loads(receipt_path.read_text())
        if any(journal.get(k)!=v for k,v in identity.items()) or journal.get('phase')!='registered':
            raise RuntimeError('Allocation already used or binding changed')
    else:
        if target.exists():raise RuntimeError('Allocation directory lacks a trusted record')
        journal=dict(identity,phase='registered',sandbox_id=instance)
        save_receipt(receipt_path,journal)
    if sys.argv[1]=='reserve':
        print(json.dumps(journal),flush=True);sys.exit(0)
    if target.exists():raise RuntimeError('Preparation path already exists; recover original evidence')
with prepare_directory() as temp:
    root=Path(temp)
    root.chmod(0o711)  # unprivileged runtime helpers can traverse, but cannot list this directory
    bounds={'archive':77679372,'probe':16*1024*1024,'profile':8192}
    if managed:bounds.update(input=262144,allocation=4096)
    seen=set()
    with bounded_packet(sys.stdin.buffer) as frame, tarfile.open(fileobj=frame,mode='r|') as packet:
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
    if managed:
        if binding!=json.loads((root/'allocation').read_text()) or binding['input_sha256']!=sha(root/'input'):
            raise ValueError('Allocation input or identity mismatch')
        if binding['lease_until']<=time.time() or binding['lease_until']>min(binding['workspace_until'],time.time()+32):
            raise ValueError('Expired or unbounded initial provisioning lease')
        journal['archive_sha256']=profile['sha256']
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
    if not managed:instance='oa-qual-'+uuid.uuid4().hex
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
            if managed:
                journal.update(phase='starting',runsc_sha256=sha(binary));save_receipt(receipt_path,journal)
            with stderr.open('wb') as err:
                child=subprocess.Popen([*args,'run','--bundle',str(bundle),instance],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=err)
            if managed:
                data=(root/'input').read_bytes()
                child.stdin.write(struct.pack('>I',len(data))+data);child.stdin.flush()
                del data
                (root/'input').unlink()
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
            if managed:
                if binding['lease_until']<=time.time():raise RuntimeError('Lease expired before materialization')
                journal.update(phase='ready',cgroup=limits,workload_uid=65534,input_staged=True)
                save_receipt(receipt_path,journal);print(json.dumps(journal),flush=True)
                wait_for_release(sys.stdin.buffer,binding['lease_until'],binding['workspace_until'])
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
    if managed:
        finish_managed()
    else:print(json.dumps({'qualified':True,'profile':profile['profile'],'runsc':version,'archive_sha256':profile['sha256'],
                      'probe_sha256':sha(root/'probe'),'kernel':platform.release(),'platform':'systrap','network':'none',
                      'cgroup':limits,'workspace_bytes':profile['workspace_bytes'],'workload_uid':65534,'effective_capabilities':0,
                      'root_readonly':True,'sentinel_unreachable':True,'sandbox_and_cgroup_removed':True,
                      'scope':'one owned static payload in local Linux ARM64 VM; production pool, Web Agent and GPU are separate'}))
