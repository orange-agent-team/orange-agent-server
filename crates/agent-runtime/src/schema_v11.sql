CREATE TABLE editing_publication_steps(
 operation_id TEXT NOT NULL REFERENCES editing_publications(operation_id),
 step INTEGER NOT NULL CHECK(step BETWEEN 1 AND 7),
 body TEXT NOT NULL CHECK(json_valid(body)),
 PRIMARY KEY(operation_id,step)
);
CREATE TRIGGER editing_step_sequence BEFORE INSERT ON editing_publication_steps WHEN NEW.step != (SELECT coalesce(max(step),0)+1 FROM editing_publication_steps WHERE operation_id=NEW.operation_id) BEGIN SELECT RAISE(ABORT,'publication stage out of order');END;
CREATE TRIGGER editing_step_no_update BEFORE UPDATE ON editing_publication_steps BEGIN SELECT RAISE(ABORT,'immutable publication stage');END;
CREATE TRIGGER editing_step_no_delete BEFORE DELETE ON editing_publication_steps BEGIN SELECT RAISE(ABORT,'publication stages retained');END;
PRAGMA user_version=11;
