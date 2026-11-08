-- A durable dispatch reserves names/retention capacity before filesystem
-- effects. Plans are evidence, never permission to replay an unknown operation.
CREATE TABLE editing_publications(
 operation_id TEXT PRIMARY KEY REFERENCES editing_previews(operation_id),
 resource_identity TEXT NOT NULL,
 host_epoch INTEGER NOT NULL CHECK(host_epoch>0),
 reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>0),
 body TEXT NOT NULL CHECK(json_valid(body)),
 digest TEXT NOT NULL CHECK(length(digest)=64)
);
CREATE INDEX editing_publications_resource ON editing_publications(resource_identity);
CREATE TRIGGER editing_publication_no_update BEFORE UPDATE ON editing_publications BEGIN SELECT RAISE(ABORT,'immutable publication plan');END;
CREATE TRIGGER editing_publication_no_delete BEFORE DELETE ON editing_publications BEGIN SELECT RAISE(ABORT,'publication recovery evidence retained');END;
PRAGMA user_version=10;
