-- Human confirmation does not remove a known prior data dependency. Older
-- readers treated user_declaration as source-free and must refuse this format.
CREATE TABLE runtime_memory_continuity_policy(singleton INTEGER PRIMARY KEY CHECK(singleton=1),policy TEXT NOT NULL CHECK(policy='human-source-continuity.v1'));
INSERT INTO runtime_memory_continuity_policy VALUES(1,'human-source-continuity.v1');
CREATE TRIGGER memory_continuity_no_update BEFORE UPDATE ON runtime_memory_continuity_policy BEGIN SELECT RAISE(ABORT,'immutable source continuity policy');END;
CREATE TRIGGER memory_continuity_no_delete BEFORE DELETE ON runtime_memory_continuity_policy BEGIN SELECT RAISE(ABORT,'source continuity policy retained');END;
CREATE UNIQUE INDEX memory_approved_version ON memory_drafts(scope_id,entry_id,result_version) WHERE state='approved';
PRAGMA user_version=28;
