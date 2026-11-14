-- New binaries must understand capture witnesses before opening these records.
CREATE TABLE patch_capture_witnesses(
 operation_id TEXT PRIMARY KEY REFERENCES editing_publications(operation_id),
 body TEXT NOT NULL CHECK(json_valid(body)), digest TEXT NOT NULL
);
CREATE TABLE patch_capture_receipts(
 operation_id TEXT PRIMARY KEY REFERENCES editing_publications(operation_id),
 security_digest TEXT NOT NULL
);
CREATE TRIGGER capture_witness_before_move BEFORE INSERT ON patch_capture_witnesses WHEN (SELECT coalesce(max(step),0) FROM editing_publication_steps WHERE operation_id=NEW.operation_id)!=1 OR NOT EXISTS(SELECT 1 FROM operations o JOIN editing_publications p USING(operation_id) JOIN runtime_host h ON h.singleton=1 WHERE o.operation_id=NEW.operation_id AND o.state='dispatching' AND p.host_epoch=h.epoch) BEGIN SELECT RAISE(ABORT,'capture witness must precede capture');END;
CREATE TRIGGER capture_receipt_after_intent BEFORE INSERT ON patch_capture_receipts WHEN (SELECT coalesce(max(step),0) FROM editing_publication_steps WHERE operation_id=NEW.operation_id)!=3 OR NOT EXISTS(SELECT 1 FROM operations o JOIN editing_publications p USING(operation_id) JOIN runtime_host h ON h.singleton=1 WHERE o.operation_id=NEW.operation_id AND o.state='dispatching' AND p.host_epoch=h.epoch) BEGIN SELECT RAISE(ABORT,'capture receipt requires capture intent');END;
CREATE TRIGGER capture_witness_no_update BEFORE UPDATE ON patch_capture_witnesses BEGIN SELECT RAISE(ABORT,'immutable capture witness');END;
CREATE TRIGGER capture_witness_no_delete BEFORE DELETE ON patch_capture_witnesses BEGIN SELECT RAISE(ABORT,'capture witness retained');END;
CREATE TRIGGER capture_receipt_no_update BEFORE UPDATE ON patch_capture_receipts BEGIN SELECT RAISE(ABORT,'immutable capture receipt');END;
CREATE TRIGGER capture_receipt_no_delete BEFORE DELETE ON patch_capture_receipts BEGIN SELECT RAISE(ABORT,'capture receipt retained');END;
PRAGMA user_version=17;
