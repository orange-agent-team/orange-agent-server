-- Copy recovery never erases the capsule or overwrites an existing destination.
CREATE TABLE export_restorations(
 restoration_id TEXT PRIMARY KEY, retirement_id TEXT NOT NULL REFERENCES export_retirements(retirement_id),
 owner TEXT NOT NULL, request_key TEXT NOT NULL, host_epoch INTEGER NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body)), digest TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN('prepared','dispatching','succeeded','unknown')),
 receipt TEXT, receipt_digest TEXT, UNIQUE(owner,request_key),
 CHECK((state='succeeded' AND receipt IS NOT NULL AND json_valid(receipt) AND receipt_digest IS NOT NULL)
 OR(state!='succeeded' AND receipt IS NULL AND receipt_digest IS NULL))
);
CREATE TRIGGER restoration_current BEFORE INSERT ON export_restorations WHEN NEW.state!='prepared' OR NOT EXISTS(
 SELECT 1 FROM export_retirements r JOIN runtime_host h ON h.singleton=1 WHERE r.retirement_id=NEW.retirement_id AND r.owner=NEW.owner AND r.state IN('retained','unknown') AND h.epoch=NEW.host_epoch)
 BEGIN SELECT RAISE(ABORT,'restoration requires current owned terminal retirement');END;
CREATE TRIGGER restoration_binding BEFORE UPDATE ON export_restorations WHEN NEW.restoration_id!=OLD.restoration_id OR NEW.retirement_id!=OLD.retirement_id OR NEW.owner!=OLD.owner OR NEW.request_key!=OLD.request_key OR NEW.host_epoch!=OLD.host_epoch OR NEW.body!=OLD.body OR NEW.digest!=OLD.digest
 BEGIN SELECT RAISE(ABORT,'immutable restoration binding');END;
CREATE TRIGGER restoration_transition BEFORE UPDATE ON export_restorations WHEN NOT((OLD.state='prepared' AND NEW.state='dispatching') OR(OLD.state='dispatching' AND NEW.state IN('succeeded','unknown')))
 BEGIN SELECT RAISE(ABORT,'restoration must not replay');END;
CREATE TRIGGER restoration_no_delete BEFORE DELETE ON export_restorations BEGIN SELECT RAISE(ABORT,'restoration record retained');END;
PRAGMA user_version=21;
