-- Version transitions are evidence of confirmed effects, never intent or
-- approval. Old successful publications do not acquire invented identities.
CREATE TABLE patch_version_edges(
 operation_id TEXT PRIMARY KEY REFERENCES editing_publications(operation_id),
 event_seq INTEGER NOT NULL UNIQUE REFERENCES events(seq),
 body TEXT NOT NULL CHECK(json_valid(body)),digest TEXT NOT NULL CHECK(length(digest)=64)
);
CREATE TRIGGER patch_edge_confirmed BEFORE INSERT ON patch_version_edges WHEN NOT EXISTS(
 SELECT 1 FROM operations o JOIN events e ON e.operation_id=o.operation_id
 JOIN editing_publication_steps p ON p.operation_id=o.operation_id AND p.step=7
 WHERE o.operation_id=NEW.operation_id AND o.state='succeeded' AND o.tool_version='apply_patch.v1'
 AND e.seq=NEW.event_seq AND e.run_id=o.run_id AND e.kind='operation.succeeded')
 BEGIN SELECT RAISE(ABORT,'version edge requires confirmed publication');END;
CREATE TRIGGER patch_edge_no_update BEFORE UPDATE ON patch_version_edges BEGIN SELECT RAISE(ABORT,'immutable version transition');END;
CREATE TRIGGER patch_edge_no_delete BEFORE DELETE ON patch_version_edges BEGIN SELECT RAISE(ABORT,'version transition retained');END;
PRAGMA user_version=14;
