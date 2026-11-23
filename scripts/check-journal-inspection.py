#!/usr/bin/env python3
"""A closed writer can leave committed pages only in its WAL. Inspection must preserve every source byte."""
import hashlib
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path
from journal_schema import inspect_closed_journal


def inventory(directory):
    return {file.name:hashlib.sha256(file.read_bytes()).hexdigest() for file in directory.iterdir()}


def main():
    with tempfile.TemporaryDirectory() as tmp:
        directory = Path(tmp)
        database = directory / "journal.sqlite"
        with sqlite3.connect(database) as setup:
            setup.execute("CREATE TABLE journal_messages (message TEXT NOT NULL)")
        setup.close()
        # Exit with a live connection so SQLite cannot checkpoint on final close.
        writer = """
import os, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
if db.execute('PRAGMA journal_mode=WAL').fetchone()[0] != 'wal':
    raise RuntimeError('WAL expected')
db.execute('PRAGMA wal_autocheckpoint=0')
db.execute("INSERT INTO journal_messages VALUES ('committed')")
db.commit()
db.execute("INSERT INTO journal_messages VALUES ('uncommitted')")
os._exit(0)
"""
        subprocess.run([sys.executable,"-c",writer,database],check=True)
        before = inventory(directory)
        if database.name+"-wal" not in before:
            raise ValueError("Writer did not leave a WAL")
        with inspect_closed_journal(database) as db:
            if db.execute("SELECT message FROM journal_messages").fetchall() != [("committed",)]:
                raise ValueError("Incorrect committed snapshot")
            try:
                db.execute("DELETE FROM journal_messages")
            except sqlite3.OperationalError as exc:
                if "readonly" not in str(exc).lower():
                    raise
            else:
                raise ValueError("Snapshot accepted a write")
        if inventory(directory)!=before:
            raise ValueError("Inspection modified source journal files")
    print("PASS committed WAL recovery, exclusion of unfinished writes, query-only snapshot and identical source byte inventory")


if __name__=="__main__":
    main()
