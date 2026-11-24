"""Reopen and withdraw the actual on-disk knowledge index, with independent process and SQLite checks."""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time

def require(ok,msg):
    if not ok:raise ValueError(msg)

def verify_cache(binary,Peer):
    with tempfile.TemporaryDirectory(prefix="orange-knowledge-restart-") as fixture:
        root=Path(fixture)/"docs";root.mkdir(mode=0o700)
        private=Path(fixture)/"private";private.mkdir(mode=0o700)
        authority=Path(fixture)/"authority";authority.mkdir(mode=0o700)
        called="secret-project-authorization.txt"
        sentinel="PROPRIETARYQASENTINEL"
        selected=root/called
        selected.write_text(f"{sentinel} launches in April. 四月启航。")
        other=root/"second.txt";other.write_text("Another selected source.")
        denied=root/"unselected.txt";denied.write_text("Unselected confidential note.")
        names=[called,other.name,"bulk-crash.txt"]
        def open_peer(paths=names):
            peer=Peer(binary,"reader",root,private,paths,persistent=True,authority=authority)
            peer.start();return peer
        def invoke(*args):
            return subprocess.run([binary,*args],input="",text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=15)
        peer=open_peer()
        try:
            hit=peer.search(sentinel)["hits"][0]
            require(peer.read(hit)["structuredContent"]["data"]["text"]==selected.read_text(),"Initial citation")
            require(invoke("--withdraw-saved","reader","manuals",private,authority).returncode!=0,"Live writer lock ineffective")
        finally:peer.close()
        file,=list(private.glob("*.sqlite"))
        def facts():
            with sqlite3.connect(file.as_uri()+"?mode=ro",uri=True) as db:
                return db.execute("SELECT active FROM binding").fetchone()[0],db.execute("SELECT count(*) FROM issued").fetchone()[0],db.execute("SELECT count(*) FROM docs").fetchone()[0],hashlib.sha256(b"".join(r[0] for r in db.execute("SELECT block FROM chunks_data ORDER BY id"))).hexdigest()
        initial=facts()
        previous_cache=file.read_bytes()
        require(initial[:3]==(1,1,2),"Missing durable version witnesses")
        # Include journal/lock files as well as the database in the cleartext check.
        for f in private.iterdir():
            if f.is_file():
                b=f.read_bytes()
                require(all(v not in b for v in (sentinel.encode(),sentinel.lower().encode(),called.encode(),"四月".encode())), "Source text, terms or path present in persisted artifact")
        mismatch=invoke("--persist","--authority-dir",authority,"reader","manuals",root,private,called,denied.name)
        require(mismatch.returncode!=0 and facts()==initial,"Changed selection opened or altered prior state")
        peer=open_peer()
        try:
            content=peer.read(hit)["structuredContent"]["data"];require(content["text"]==selected.read_text(),"Direct read after authorized reopen")
            require(peer.search(sentinel)["hits"][0]==hit,"Restart changed a surviving citation")
        finally:peer.close()
        require(facts()==initial,"Unchanged source unexpectedly rebuilt the persisted posting segments")
        # Interrupt an actual hot rollback journal, then force SQLite recovery on reopening.
        bulk=root/"bulk-crash.txt"
        bulk.write_text(("sensitivecoldposting alpha beta 年度计划\n"*5500)[:52000])
        interrupted=open_peer()
        frame={"jsonrpc":"2.0","id":900,"method":"tools/call","params":{"name":"knowledge_search","arguments":{"collection_id":"manuals","query":"alpha","path":bulk.name}}}
        interrupted.process.stdin.write(json.dumps(frame)+"\n");interrupted.process.stdin.flush()
        journal=Path(str(file)+"-journal");until=time.monotonic()+10
        try:
            while time.monotonic()<until:
                try:
                    if journal.stat().st_size>0:break
                except FileNotFoundError:pass
                time.sleep(.001)
            else:raise ValueError("No interrupted write transaction was observed")
        finally:
            interrupted.process.kill();interrupted.process.wait(timeout=10);interrupted.process.stdin.close();interrupted.process.stdout.close()
        bulk.unlink()
        peer=open_peer()
        try:require(peer.read(hit)["structuredContent"]["data"]["text"]==selected.read_text(),"Interrupted index update corrupted a preceding receipt")
        finally:peer.close()
        with sqlite3.connect(file.as_uri()+"?mode=ro",uri=True) as db:
            require(db.execute("PRAGMA integrity_check").fetchone()==("ok",),"Failed to recover local index")
        replacement=root/"tmp.txt";replacement.write_bytes(selected.read_bytes());os.replace(replacement,selected)
        peer=open_peer()
        try:
            require(peer.read(hit)["isError"],"Byte-identical replacement recovered old content")
            new=peer.search(sentinel)["hits"][0];require(new["hit_id"]!=hit["hit_id"],"Replaced document reused old hit")
        finally:peer.close()
        selected.unlink()
        peer=open_peer()
        try:
            require(peer.read(new)["isError"],"Deleted document returned from persistent state")
            require(not peer.search(sentinel)["hits"],"Deleted version still searchable")
        finally:peer.close()
        require(facts()[:3]==(1,0,1),"Deletion did not purge issued and version rows")
        for _ in range(2):
            receipt=invoke("--withdraw-saved","reader","manuals",private,authority)
            require(receipt.returncode==0 and json.loads(receipt.stdout)["state"]=="withdrawn","Non-idempotent withdrawal")
        require(facts()[:3]==(0,0,0),"Withdrawn cache contains live source metadata")
        require(invoke("--persist","--authority-dir",authority,"reader","manuals",root,private,*names).returncode!=0,"Withdrawn store reopened")
        # Restoring a valid older cache cannot roll back its separately committed authority.
        file.write_bytes(previous_cache)
        require(invoke("--persist","--authority-dir",authority,"reader","manuals",root,private,*names).returncode!=0,"Restored database revived withdrawn authority")
        require(facts()[:3]==(0,0,0),"Restored closed cache was not scrubbed on access")
        file.write_bytes(previous_cache)
        missing_authority=Path(fixture)/"missing-authority";missing_authority.mkdir(mode=0o700)
        require(invoke("--persist","--authority-dir",missing_authority,"reader","manuals",root,private,*names).returncode!=0,"Missing authority was treated as a new index")
        require(not list(missing_authority.iterdir()),"A cache backup minted a replacement authority")
        require(invoke("--withdraw-saved","reader","manuals",private,authority).returncode==0 and facts()[:3]==(0,0,0),"Idempotent purge of restored cache failed")
        if os.name=="posix":
            original_members=set(private.iterdir())
            private.chmod(0o755)
            try:require(invoke("--persist","reader","different",root,private,*names).returncode!=0,"Nonprivate directory accepted")
            finally:private.chmod(0o700)
            require(set(private.iterdir())==original_members,"Refusal created files outside the privacy boundary")
        print(json.dumps({"persistent_restart_read":True,"unchanged_posting_segments_reused":True,"cleartext_sentinels_found":0,
            "offline_deleted_and_replaced_hits_denied":True,"wrong_selection_denied":True,"revocation_survived_restart":True,"concurrent_withdrawal_refused_while_owned":True,"interrupted_write_recovered":True,"stale_cache_restore_rejected":True,"authority_loss_refused":True}))
        print("PASS durable knowledge witnesses, unchanged full-text postings, live reopen and irreversible store withdrawal")
