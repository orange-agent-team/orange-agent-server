CREATE TABLE saved_knowledge_collections(
    workspace_id TEXT NOT NULL REFERENCES local_workspaces(workspace_id),
    name TEXT NOT NULL,
    store_id TEXT NOT NULL UNIQUE,
    permission_version INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('active','retiring','retired')),
    PRIMARY KEY(workspace_id,name)
);
CREATE INDEX saved_knowledge_pending ON saved_knowledge_collections(workspace_id) WHERE state='retiring';
CREATE TRIGGER retire_workspace_saved_knowledge AFTER UPDATE OF active ON local_workspaces
WHEN OLD.active AND NOT NEW.active
BEGIN
    UPDATE saved_knowledge_collections SET state='retiring' WHERE workspace_id=NEW.workspace_id AND state='active';
END;
PRAGMA user_version=36;
