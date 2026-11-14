-- Explicit retained-file export; no relation to Agent tool replay or archive disposal.
CREATE TABLE patch_exports(
 export_id TEXT PRIMARY KEY,
 operation_id TEXT NOT NULL REFERENCES editing_publications(operation_id),
 owner TEXT NOT NULL, request_key TEXT NOT NULL,
 host_epoch INTEGER NOT NULL CHECK(host_epoch>0), expires_at INTEGER NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body)), digest TEXT NOT NULL CHECK(length(digest)=64),
 state TEXT NOT NULL CHECK(state IN('prepared','dispatching','succeeded','unknown')),
 receipt TEXT, receipt_digest TEXT,
 UNIQUE(owner,request_key),
 CHECK((state='succeeded' AND receipt IS NOT NULL AND json_valid(receipt) AND receipt_digest IS NOT NULL AND length(receipt_digest)=64)
    OR (state!='succeeded' AND receipt IS NULL AND receipt_digest IS NULL))
);
CREATE TRIGGER patch_export_owned_insert BEFORE INSERT ON patch_exports WHEN NEW.state!='prepared' OR NOT EXISTS(
 SELECT 1 FROM operations o JOIN runtime_runs r ON r.run_id=o.run_id JOIN runtime_host h ON h.singleton=1
 WHERE o.operation_id=NEW.operation_id AND o.owner=NEW.owner AND h.epoch=NEW.host_epoch
 AND (o.state='succeeded' OR (o.state='unknown' AND EXISTS(SELECT 1 FROM patch_recovery_resolutions x WHERE x.operation_id=o.operation_id))))
 BEGIN SELECT RAISE(ABORT,'export requires current owned retained publication');END;
CREATE TRIGGER patch_export_binding_immutable BEFORE UPDATE ON patch_exports WHEN
 NEW.export_id!=OLD.export_id OR NEW.operation_id!=OLD.operation_id OR NEW.owner!=OLD.owner OR NEW.request_key!=OLD.request_key
 OR NEW.host_epoch!=OLD.host_epoch OR NEW.expires_at!=OLD.expires_at OR NEW.body!=OLD.body OR NEW.digest!=OLD.digest
 BEGIN SELECT RAISE(ABORT,'immutable export binding');END;
CREATE TRIGGER patch_export_transition BEFORE UPDATE ON patch_exports WHEN NOT(
 (OLD.state='prepared' AND NEW.state='dispatching') OR
 (OLD.state='dispatching' AND NEW.state IN('succeeded','unknown')))
 BEGIN SELECT RAISE(ABORT,'export must not replay');END;
CREATE TRIGGER patch_export_no_delete BEFORE DELETE ON patch_exports BEGIN SELECT RAISE(ABORT,'export records retained');END;
PRAGMA user_version=18;
