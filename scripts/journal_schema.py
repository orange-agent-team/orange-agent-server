"""Independent release expectation for legacy-to-current SQLite acceptance."""
from contextlib import contextmanager
import json
import shutil
import sqlite3
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def verify_current_journal(db):
    expected = json.loads((ROOT / "config/journal-schema.json").read_bytes())["user_version"]
    actual = db.execute("PRAGMA user_version").fetchone()[0]
    assert type(expected) is int and expected >= 31 and actual == expected, (actual, expected)
    required = {("table", "local_skill_packages"), ("table", "local_skill_selections"),
                ("table", "local_run_skills"), ("trigger", "local_run_skill_owner"),
                ("trigger", "immutable_local_run_skill"), ("view", "available_skill_versions")}
    objects = set(db.execute("SELECT type, name FROM sqlite_schema"))
    assert required <= objects, required - objects
    return actual


@contextmanager
def inspect_closed_journal(database):
    """Read committed pages from a private replica of a fixture after its prior process exited."""
    with tempfile.TemporaryDirectory(prefix="journal-inspection-") as copy_dir:
        replica = Path(copy_dir) / database.name
        shutil.copyfile(database, replica)
        for suffix in ("-wal", "-journal"):
            sidecar = database.with_name(database.name + suffix)
            if sidecar.exists():
                shutil.copyfile(sidecar, replica.with_name(replica.name + suffix))
        db = sqlite3.connect(replica)
        try:
            db.execute("PRAGMA query_only=ON")
            yield db
        finally:
            db.close()
