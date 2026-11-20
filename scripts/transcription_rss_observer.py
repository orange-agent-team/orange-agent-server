"""QA-only exit observation in the actual E09 interpreter; keep original pid, streams and runner code."""
import atexit
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import uuid

if sys.platform != "darwin" or sys.argv[1:4] != ["-I","-B","-c"] or len(sys.argv)!=7:
    raise SystemExit("Expected the fixed isolated CPU acceptance entry")
_, _, _, _, source, model, slot_name = sys.argv
slot=Path(slot_name)
expected_parent=os.getppid()


def observe_exit():
    pid=os.getpid()
    if os.getppid()!=expected_parent:return
    marker=json.loads((slot/"last-observed.json").read_bytes())
    if marker["pid"]!=pid or str(uuid.UUID(marker["attempt_id"]))!=marker["attempt_id"]:
        raise ValueError("Missing real backend attempt")
    output=subprocess.check_output(["/bin/ps","-o","pid=,ppid=,rss=","-p",f"{expected_parent},{pid}"],text=True)
    rows={p:(pp,rss) for p,pp,rss in (tuple(map(int,line.split())) for line in output.splitlines())}
    if rows[pid][0]!=expected_parent:raise ValueError("Backend no longer belongs to measured host")
    combined=(rows[expected_parent][1]+rows[pid][1])*1024
    high=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss  # Darwin returns bytes.
    if combined<=0 or high<=0:raise ValueError("Kernel did not return resource measurements")
    sample={"pid":pid,"parent":expected_parent,"attempt":marker["attempt_id"],"combined_rss_bytes":combined,"backend_peak_rss_bytes":high}
    with (slot/("rss-"+marker["attempt_id"]+".json")).open("x") as f:
        json.dump(sample,f)


atexit.register(observe_exit)
sys.argv=["-c",model,slot_name]
exec(compile(source,"<string>","exec"),{"__name__":"__main__","__builtins__":__builtins__})
