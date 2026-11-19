CREATE TABLE memory_owner_removals (
 scope_id TEXT NOT NULL REFERENCES memory_scopes(scope_id),
 request_id TEXT NOT NULL,
 actor TEXT NOT NULL,
 scope_revision INTEGER NOT NULL CHECK(scope_revision>0),
 entry_id TEXT NOT NULL,
 entry_version INTEGER NOT NULL CHECK(entry_version>0),
 expires_at INTEGER NOT NULL,
 digest TEXT NOT NULL,
 applied INTEGER NOT NULL CHECK(applied IN(0,1)),
 PRIMARY KEY(scope_id,request_id),
 FOREIGN KEY(scope_id,entry_id) REFERENCES memory_entries(scope_id,entry_id)
);
CREATE TRIGGER memory_owner_removal_receipt BEFORE UPDATE ON memory_owner_removals
WHEN OLD.applied!=0 OR NEW.applied!=1 OR NEW.scope_id!=OLD.scope_id OR NEW.request_id!=OLD.request_id
 OR NEW.actor!=OLD.actor OR NEW.scope_revision!=OLD.scope_revision OR NEW.entry_id!=OLD.entry_id
 OR NEW.entry_version!=OLD.entry_version OR NEW.expires_at!=OLD.expires_at OR NEW.digest!=OLD.digest
BEGIN SELECT RAISE(ABORT,'immutable owner removal intent'); END;
CREATE TRIGGER memory_owner_removal_no_receipt_delete BEFORE DELETE ON memory_owner_removals
WHEN OLD.applied=1 BEGIN SELECT RAISE(ABORT,'owner removal receipt retained'); END;
PRAGMA user_version=29;
