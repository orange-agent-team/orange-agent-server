CREATE TABLE broker_meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1),epoch INTEGER NOT NULL CHECK(epoch>0),config_digest TEXT NOT NULL,owner TEXT NOT NULL,remote_allowed INTEGER NOT NULL DEFAULT 0 CHECK(remote_allowed IN(0,1)));
CREATE TABLE supervisors(instance_id TEXT PRIMARY KEY,owner TEXT NOT NULL,process_identity TEXT NOT NULL,nonce TEXT NOT NULL,epoch INTEGER NOT NULL,credential_digest TEXT NOT NULL);
CREATE TABLE grants(
 grant_id TEXT PRIMARY KEY,device_id TEXT NOT NULL,epoch INTEGER NOT NULL,generation INTEGER NOT NULL,
 supervisor TEXT NOT NULL REFERENCES supervisors(instance_id),mode TEXT NOT NULL CHECK(mode IN('local','remote')),
 profile_id TEXT NOT NULL,remote_binding TEXT,remote_deadline_ms INTEGER,
 state TEXT NOT NULL CHECK(state IN('reserved','running','draining','released','revoked','quarantined')),
 expires_ms INTEGER NOT NULL,UNIQUE(device_id,generation)
);
CREATE TABLE slots(
 device_id TEXT PRIMARY KEY,state TEXT NOT NULL CHECK(state IN('reconciling','idle','reserved_local','reserved_remote','running_local','running_remote','draining','quarantined')),
 epoch INTEGER NOT NULL,generation INTEGER NOT NULL DEFAULT 0,grant_id TEXT REFERENCES grants(grant_id),
 local_pending INTEGER NOT NULL DEFAULT 0 CHECK(local_pending IN(0,1)),reason TEXT
);
CREATE TABLE broker_commands(request_id TEXT PRIMARY KEY,supervisor TEXT NOT NULL,kind TEXT NOT NULL,digest TEXT NOT NULL,result TEXT NOT NULL);
CREATE TABLE broker_events(seq INTEGER PRIMARY KEY AUTOINCREMENT,device_id TEXT,grant_id TEXT,kind TEXT NOT NULL,details TEXT NOT NULL);
CREATE TRIGGER broker_events_no_update BEFORE UPDATE ON broker_events BEGIN SELECT RAISE(ABORT,'immutable broker event'); END;
CREATE TRIGGER broker_events_no_delete BEFORE DELETE ON broker_events BEGIN SELECT RAISE(ABORT,'immutable broker event'); END;
CREATE TRIGGER broker_commands_no_update BEFORE UPDATE ON broker_commands BEGIN SELECT RAISE(ABORT,'immutable broker command'); END;
CREATE TRIGGER broker_commands_no_delete BEFORE DELETE ON broker_commands BEGIN SELECT RAISE(ABORT,'immutable broker command'); END;
PRAGMA user_version=1;
