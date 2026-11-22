"""Independent release expectation for legacy-to-current SQLite acceptance."""
import json
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
