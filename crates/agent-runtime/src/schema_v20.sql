-- Reversible retirement only: no unlink, success repair or quota release.
CREATE TABLE export_retirements(
 retirement_id TEXT PRIMARY KEY, export_id TEXT NOT NULL REFERENCES patch_exports(export_id),
 owner TEXT NOT NULL, request_key TEXT NOT NULL, host_epoch INTEGER NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body)), digest TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN('prepared','dispatching','retained','unknown')),
 receipt TEXT, receipt_digest TEXT, UNIQUE(owner,request_key),
 CHECK((state='retained' AND receipt IS NOT NULL AND json_valid(receipt) AND receipt_digest IS NOT NULL)
 OR (state!='retained' AND receipt IS NULL AND receipt_digest IS NULL))
);
CREATE TABLE export_retirement_capsules(
 retirement_id TEXT PRIMARY KEY REFERENCES export_retirements(retirement_id),
 body TEXT NOT NULL CHECK(json_valid(body)), digest TEXT NOT NULL
);
CREATE TRIGGER retirement_current BEFORE INSERT ON export_retirements WHEN NEW.state!='prepared' OR NOT EXISTS(
 SELECT 1 FROM patch_exports e JOIN runtime_host h ON h.singleton=1 WHERE e.export_id=NEW.export_id AND e.owner=NEW.owner AND h.epoch=NEW.host_epoch AND e.state IN('unknown','succeeded'))
 BEGIN SELECT RAISE(ABORT,'retirement requires current owned terminal export');END;
CREATE TRIGGER retirement_binding BEFORE UPDATE ON export_retirements WHEN NEW.retirement_id!=OLD.retirement_id OR NEW.export_id!=OLD.export_id OR NEW.owner!=OLD.owner OR NEW.request_key!=OLD.request_key OR NEW.host_epoch!=OLD.host_epoch OR NEW.body!=OLD.body OR NEW.digest!=OLD.digest
 BEGIN SELECT RAISE(ABORT,'immutable retirement binding');END;
CREATE TRIGGER retirement_transition BEFORE UPDATE ON export_retirements WHEN NOT((OLD.state='prepared' AND NEW.state='dispatching') OR (OLD.state='dispatching' AND NEW.state IN('retained','unknown')))
 BEGIN SELECT RAISE(ABORT,'retirement must not replay');END;
CREATE TRIGGER retirement_no_delete BEFORE DELETE ON export_retirements BEGIN SELECT RAISE(ABORT,'retirement record retained');END;
CREATE TRIGGER capsule_current BEFORE INSERT ON export_retirement_capsules WHEN NOT EXISTS(SELECT 1 FROM export_retirements r JOIN runtime_host h ON h.singleton=1 WHERE r.retirement_id=NEW.retirement_id AND r.state='dispatching' AND r.host_epoch=h.epoch)
 BEGIN SELECT RAISE(ABORT,'capsule requires current dispatch');END;
CREATE TRIGGER capsule_no_update BEFORE UPDATE ON export_retirement_capsules BEGIN SELECT RAISE(ABORT,'immutable capsule identity');END;
CREATE TRIGGER capsule_no_delete BEFORE DELETE ON export_retirement_capsules BEGIN SELECT RAISE(ABORT,'capsule evidence retained');END;
PRAGMA user_version=20;
