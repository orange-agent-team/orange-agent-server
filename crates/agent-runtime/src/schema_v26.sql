-- A discovery result may cite several entries. Existing single-entry references
-- migrate without changing their bytes, versions or authority bindings.
DROP TRIGGER memory_read_binding;
DROP TRIGGER memory_read_no_delete;
ALTER TABLE runtime_memory_sources RENAME TO runtime_memory_sources_v25;
CREATE TABLE runtime_memory_sources(
 operation_id TEXT NOT NULL REFERENCES operations(operation_id),
 scope_id TEXT NOT NULL,
 entry_id TEXT NOT NULL,
 entry_version INTEGER NOT NULL CHECK(entry_version>0),
 PRIMARY KEY(operation_id,scope_id,entry_id),
 FOREIGN KEY(scope_id,entry_id) REFERENCES memory_entries(scope_id,entry_id)
);
INSERT INTO runtime_memory_sources SELECT operation_id,scope_id,entry_id,entry_version FROM runtime_memory_sources_v25;
DROP TABLE runtime_memory_sources_v25;
CREATE TRIGGER memory_read_binding BEFORE UPDATE ON runtime_memory_sources BEGIN SELECT RAISE(ABORT,'immutable memory source version');END;
CREATE TRIGGER memory_read_no_delete BEFORE DELETE ON runtime_memory_sources BEGIN SELECT RAISE(ABORT,'memory source reference retained');END;
CREATE TRIGGER memory_read_source_scope BEFORE INSERT ON runtime_memory_sources
WHEN NOT EXISTS(SELECT 1 FROM runtime_memory_operations WHERE operation_id=NEW.operation_id AND scope_id=NEW.scope_id)
BEGIN SELECT RAISE(ABORT,'memory source scope does not match operation');END;
CREATE TRIGGER memory_read_source_bound BEFORE INSERT ON runtime_memory_sources
WHEN (SELECT count(*) FROM runtime_memory_sources WHERE operation_id=NEW.operation_id)>=100
BEGIN SELECT RAISE(ABORT,'memory source set limit');END;
PRAGMA user_version=26;
