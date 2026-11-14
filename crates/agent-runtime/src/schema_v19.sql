-- Persist an original private lock inode before any export destination effect.
-- Older exports without a binding remain inspectable, not certified quiescent.
CREATE TABLE export_writer_bindings(
 export_id TEXT PRIMARY KEY REFERENCES patch_exports(export_id),
 body TEXT NOT NULL CHECK(json_valid(body)),
 digest TEXT NOT NULL CHECK(length(digest)=64)
);
CREATE TRIGGER export_writer_current_insert BEFORE INSERT ON export_writer_bindings WHEN NOT EXISTS(
 SELECT 1 FROM patch_exports e JOIN runtime_host h ON h.singleton=1
 WHERE e.export_id=NEW.export_id AND e.state='dispatching' AND e.host_epoch=h.epoch)
 BEGIN SELECT RAISE(ABORT,'writer binding requires current export dispatch');END;
CREATE TRIGGER export_writer_no_update BEFORE UPDATE ON export_writer_bindings BEGIN SELECT RAISE(ABORT,'immutable writer identity');END;
CREATE TRIGGER export_writer_no_delete BEFORE DELETE ON export_writer_bindings BEGIN SELECT RAISE(ABORT,'writer evidence retained');END;
PRAGMA user_version=19;
