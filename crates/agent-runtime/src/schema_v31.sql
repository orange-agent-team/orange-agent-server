CREATE TABLE local_skill_packages (
 owner TEXT NOT NULL, package_id TEXT NOT NULL, version TEXT NOT NULL,
 digest TEXT NOT NULL, installation_revision INTEGER NOT NULL CHECK(installation_revision>0), document TEXT, installed INTEGER NOT NULL CHECK(installed IN (0,1)),
 PRIMARY KEY(owner,package_id,version), UNIQUE(owner,package_id,digest),
 CHECK(installed=0 OR document IS NOT NULL)
);
CREATE TRIGGER preserve_local_skill_identity BEFORE UPDATE OF owner,package_id,version,digest ON local_skill_packages
BEGIN SELECT RAISE(ABORT,'immutable reviewed package identity'); END;
CREATE TABLE local_skill_selections (
 owner TEXT NOT NULL, package_id TEXT NOT NULL, digest TEXT NOT NULL,
 revision INTEGER NOT NULL CHECK(revision>0), enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
 PRIMARY KEY(owner,package_id),
 FOREIGN KEY(owner,package_id,digest) REFERENCES local_skill_packages(owner,package_id,digest)
);
CREATE TABLE local_run_skills (
 run_id TEXT PRIMARY KEY REFERENCES runtime_runs(run_id) ON DELETE CASCADE,
 owner TEXT NOT NULL, package_id TEXT NOT NULL, digest TEXT NOT NULL, activation_revision INTEGER NOT NULL CHECK(activation_revision>0),
 FOREIGN KEY(owner,package_id,digest) REFERENCES local_skill_packages(owner,package_id,digest)
);
CREATE TRIGGER local_run_skill_owner BEFORE INSERT ON local_run_skills
WHEN NOT EXISTS(SELECT 1 FROM runs WHERE run_id=NEW.run_id AND owner=NEW.owner)
 OR EXISTS(SELECT 1 FROM runtime_skill_bindings WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT,'conflicting run skill or owner'); END;
CREATE TRIGGER bundled_run_skill_exclusive BEFORE INSERT ON runtime_skill_bindings
WHEN EXISTS(SELECT 1 FROM local_run_skills WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT,'conflicting local run skill'); END;
CREATE TRIGGER immutable_local_run_skill BEFORE UPDATE ON local_run_skills
BEGIN SELECT RAISE(ABORT,'immutable local run skill'); END;
CREATE TRIGGER preserve_local_run_skill BEFORE DELETE ON local_run_skills
WHEN EXISTS(SELECT 1 FROM runtime_runs WHERE run_id=OLD.run_id)
BEGIN SELECT RAISE(ABORT,'cannot remove a running session template'); END;
CREATE VIEW available_skill_versions AS
 SELECT s.owner,s.package_id,v.version,s.digest,v.document,s.revision,s.enabled
 FROM owner_skills s JOIN skill_versions v USING(digest)
 UNION ALL
 SELECT s.owner,s.package_id,p.version,s.digest,p.document,s.revision,s.enabled
 FROM local_skill_selections s JOIN local_skill_packages p USING(owner,package_id,digest) WHERE p.installed;
PRAGMA user_version=31;
