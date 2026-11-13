-- Explicit user recovery decisions are separate from model tool approvals.
CREATE TABLE patch_recoveries(
 recovery_id TEXT PRIMARY KEY,
 operation_id TEXT NOT NULL REFERENCES editing_publications(operation_id),
 owner TEXT NOT NULL, request_key TEXT NOT NULL, host_epoch INTEGER NOT NULL CHECK(host_epoch>0),
 environment TEXT NOT NULL, read_environment TEXT NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body)), digest TEXT NOT NULL CHECK(length(digest)=64),
 state TEXT NOT NULL CHECK(state IN('intended','dispatching','unknown','succeeded')),
 UNIQUE(owner,request_key)
);
CREATE INDEX patch_recovery_operation ON patch_recoveries(operation_id,state);
CREATE TABLE patch_recovery_approvals(
 recovery_id TEXT PRIMARY KEY REFERENCES patch_recoveries(recovery_id),
 digest TEXT NOT NULL CHECK(length(digest)=64), actor TEXT NOT NULL, expires_at INTEGER NOT NULL
);
CREATE TABLE patch_recovery_results(
 recovery_id TEXT PRIMARY KEY REFERENCES patch_recoveries(recovery_id),
 body TEXT NOT NULL CHECK(json_valid(body)), digest TEXT NOT NULL CHECK(length(digest)=64)
);
CREATE TABLE patch_recovery_resolutions(
 operation_id TEXT PRIMARY KEY REFERENCES editing_publications(operation_id),
 recovery_id TEXT NOT NULL UNIQUE REFERENCES patch_recovery_results(recovery_id)
);
CREATE TRIGGER patch_recovery_immutable BEFORE UPDATE ON patch_recoveries WHEN
 NEW.recovery_id!=OLD.recovery_id OR NEW.operation_id!=OLD.operation_id OR NEW.owner!=OLD.owner OR NEW.request_key!=OLD.request_key OR NEW.host_epoch!=OLD.host_epoch OR NEW.environment!=OLD.environment OR NEW.read_environment!=OLD.read_environment OR NEW.body!=OLD.body OR NEW.digest!=OLD.digest
 BEGIN SELECT RAISE(ABORT,'immutable recovery decision');END;
CREATE TRIGGER patch_recovery_transition BEFORE UPDATE OF state ON patch_recoveries WHEN
 NOT((OLD.state='intended' AND NEW.state='dispatching') OR (OLD.state='dispatching' AND NEW.state IN('unknown','succeeded')))
 BEGIN SELECT RAISE(ABORT,'invalid recovery transition');END;
CREATE TRIGGER patch_recovery_no_delete BEFORE DELETE ON patch_recoveries BEGIN SELECT RAISE(ABORT,'recovery history retained');END;
CREATE TRIGGER patch_recovery_approval_no_update BEFORE UPDATE ON patch_recovery_approvals BEGIN SELECT RAISE(ABORT,'immutable recovery approval');END;
CREATE TRIGGER patch_recovery_approval_no_delete BEFORE DELETE ON patch_recovery_approvals BEGIN SELECT RAISE(ABORT,'recovery approval retained');END;
CREATE TRIGGER patch_recovery_result_no_update BEFORE UPDATE ON patch_recovery_results BEGIN SELECT RAISE(ABORT,'immutable recovery receipt');END;
CREATE TRIGGER patch_recovery_result_no_delete BEFORE DELETE ON patch_recovery_results BEGIN SELECT RAISE(ABORT,'recovery receipt retained');END;
CREATE TRIGGER patch_resolution_requires_result BEFORE INSERT ON patch_recovery_resolutions WHEN NOT EXISTS(
 SELECT 1 FROM patch_recoveries a JOIN patch_recovery_results r USING(recovery_id) WHERE a.recovery_id=NEW.recovery_id AND a.operation_id=NEW.operation_id AND a.state='succeeded')
 BEGIN SELECT RAISE(ABORT,'resolution requires confirmed recovery');END;
CREATE TRIGGER patch_resolution_no_update BEFORE UPDATE ON patch_recovery_resolutions BEGIN SELECT RAISE(ABORT,'immutable resolution');END;
CREATE TRIGGER patch_resolution_no_delete BEFORE DELETE ON patch_recovery_resolutions BEGIN SELECT RAISE(ABORT,'resolution history retained');END;
PRAGMA user_version=12;
