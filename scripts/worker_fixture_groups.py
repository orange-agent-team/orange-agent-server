"""Run every test in the unfiltered Worker suite once, separating incompatible database fixtures."""
from contextlib import nullcontext
import re
import subprocess

from owned_postgres import database_at

# These assertions require a specific database schedule. Keep the production operations,
# concurrent tasks, advisory lock witnesses, decoy waiter, deadlines and negative effects intact.
BARRIERS={
    "input_cases::withdrawals::expiry_before_barrier_commit_rolls_back_the_entire_stop":"orange_expiry_barrier_",
    "input_cases::withdrawals::submit_snapshot_started_before_waited_barrier_cannot_miss_the_barrier":"orange_order_barrier_",
}


def run_complete(command,root,environment):
    listed=subprocess.check_output([*command,"--","--list"],cwd=root,env=environment,text=True)
    all_names=[line.removesuffix(": test") for line in listed.splitlines() if line.endswith(": test")]
    if len(all_names)!=len(set(all_names)) or not set(BARRIERS).issubset(all_names):
        raise ValueError("Expected the complete Worker integration test list and both transaction schedules")
    groups=[([n for n in all_names if n not in BARRIERS],None)]+[([n],prefix) for n,prefix in BARRIERS.items()]
    attempted=[];errors=[]
    for names,prefix in groups:
        if not names:continue
        base=environment["ORANGE_TEST_DATABASE_URL"]
        context=database_at(base,prefix) if prefix else nullcontext((None,base))
        with context as (_,url):
            env=dict(environment,ORANGE_TEST_DATABASE_URL=url)
            count=None
            with subprocess.Popen([*command,"--","--exact",*names],cwd=root,env=env,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT) as child:
                for line in child.stdout:
                    print(line,end="",flush=True)
                    parsed=re.search(r"^test result: ok\. (\d+) passed; 0 failed; 0 ignored;",line)
                    if parsed:count=int(parsed[1])
                code=child.wait()
            attempted.extend(names)
            if code or count!=len(names):errors.append((prefix or "shared",code,count,len(names)))
    if sorted(attempted)!=sorted(all_names) or errors:
        raise RuntimeError("Incomplete Worker integration coverage: "+str(errors))
    print(f"PASS all {len(all_names)} discovered Worker integration cases exactly once, with independent expiry and old-snapshot databases",flush=True)
