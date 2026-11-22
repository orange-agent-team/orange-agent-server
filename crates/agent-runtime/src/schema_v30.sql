CREATE TABLE skill_versions(
 digest TEXT PRIMARY KEY, package_id TEXT NOT NULL, version TEXT NOT NULL, document TEXT NOT NULL,
 UNIQUE(package_id,version)
);
CREATE TRIGGER immutable_skill_version BEFORE UPDATE ON skill_versions
BEGIN SELECT RAISE(ABORT,'immutable skill version'); END;
CREATE TABLE owner_skills(
 owner TEXT NOT NULL,package_id TEXT NOT NULL,digest TEXT NOT NULL REFERENCES skill_versions,
 revision INTEGER NOT NULL CHECK(revision>0),enabled INTEGER NOT NULL CHECK(enabled IN(0,1)),
 PRIMARY KEY(owner,package_id)
);
CREATE TABLE runtime_skill_bindings(
 run_id TEXT PRIMARY KEY REFERENCES runtime_runs(run_id) ON DELETE CASCADE,
 digest TEXT NOT NULL REFERENCES skill_versions, activation_revision INTEGER NOT NULL CHECK(activation_revision>0)
);
CREATE TRIGGER immutable_run_skill BEFORE UPDATE ON runtime_skill_bindings
BEGIN SELECT RAISE(ABORT,'immutable run skill'); END;
CREATE TRIGGER preserve_active_run_skill BEFORE DELETE ON runtime_skill_bindings
WHEN EXISTS(SELECT 1 FROM runtime_runs WHERE run_id=OLD.run_id)
BEGIN SELECT RAISE(ABORT,'cannot widen a running session by deleting its template'); END;
PRAGMA user_version=30;
