-- Retention in place: atomic accounting handoff, no filesystem deletion/move.
CREATE TABLE patch_retention_reviews(
 review_id TEXT PRIMARY KEY, operation_id TEXT NOT NULL REFERENCES editing_publications(operation_id),
 owner TEXT NOT NULL, request_key TEXT NOT NULL, host_epoch INTEGER NOT NULL CHECK(host_epoch>0),
 expires_at INTEGER NOT NULL, body TEXT NOT NULL CHECK(json_valid(body)),
 digest TEXT NOT NULL CHECK(length(digest)=64), UNIQUE(owner,request_key)
);
CREATE TABLE patch_retention_archives(
 operation_id TEXT PRIMARY KEY REFERENCES editing_publications(operation_id),
 review_id TEXT NOT NULL UNIQUE REFERENCES patch_retention_reviews(review_id),
 owner TEXT NOT NULL, resource_identity TEXT NOT NULL,
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes BETWEEN 0 AND 2883584),
 body TEXT NOT NULL CHECK(json_valid(body)), digest TEXT NOT NULL CHECK(length(digest)=64)
);
CREATE INDEX patch_retention_owner ON patch_retention_archives(owner);
CREATE TRIGGER patch_retention_review_no_update BEFORE UPDATE ON patch_retention_reviews BEGIN SELECT RAISE(ABORT,'immutable retention review');END;
CREATE TRIGGER patch_retention_review_no_delete BEFORE DELETE ON patch_retention_reviews BEGIN SELECT RAISE(ABORT,'retention review retained');END;
CREATE TRIGGER patch_retention_archive_no_update BEFORE UPDATE ON patch_retention_archives BEGIN SELECT RAISE(ABORT,'immutable archive receipt');END;
CREATE TRIGGER patch_retention_archive_no_delete BEFORE DELETE ON patch_retention_archives BEGIN SELECT RAISE(ABORT,'archive accounting retained');END;
CREATE TRIGGER patch_archive_terminal_owner BEFORE INSERT ON patch_retention_archives WHEN NOT EXISTS(
 SELECT 1 FROM operations o JOIN editing_publications p USING(operation_id) JOIN patch_retention_reviews v ON v.review_id=NEW.review_id
 WHERE o.operation_id=NEW.operation_id AND o.owner=NEW.owner AND p.resource_identity=NEW.resource_identity
 AND v.owner=NEW.owner AND v.operation_id=o.operation_id
 AND (o.state='succeeded' OR (o.state='unknown' AND EXISTS(SELECT 1 FROM patch_recovery_resolutions s WHERE s.operation_id=o.operation_id))))
 BEGIN SELECT RAISE(ABORT,'archive requires owned confirmed disposition');END;
PRAGMA user_version=13;
