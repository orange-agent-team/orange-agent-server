CREATE TABLE runtime_memory_protocol(run_id TEXT PRIMARY KEY REFERENCES runtime_runs(run_id),read_version INTEGER NOT NULL CHECK(read_version IN(1,2)));
INSERT INTO runtime_memory_protocol SELECT run_id,1 FROM runtime_runs;
CREATE TRIGGER memory_protocol_immutable BEFORE UPDATE ON runtime_memory_protocol BEGIN SELECT RAISE(ABORT,'immutable Run memory protocol');END;
CREATE TRIGGER memory_protocol_retained BEFORE DELETE ON runtime_memory_protocol BEGIN SELECT RAISE(ABORT,'Run memory protocol retained');END;
PRAGMA user_version=27;
