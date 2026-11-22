CREATE TABLE runtime_model_observations(
    invocation TEXT PRIMARY KEY REFERENCES runtime_invocations(invocation) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('call_time_limit','gateway_failed','run_deadline','host_reopened')),
    observed_at INTEGER NOT NULL,
    wait_limit_ms INTEGER,
    elapsed_ms INTEGER,
    CHECK((kind='host_reopened' AND wait_limit_ms IS NULL AND elapsed_ms IS NULL)
       OR (kind!='host_reopened' AND wait_limit_ms IS NOT NULL AND wait_limit_ms BETWEEN 0 AND 120000 AND elapsed_ms IS NOT NULL AND elapsed_ms>=0))
);
CREATE TRIGGER immutable_model_observation BEFORE UPDATE ON runtime_model_observations
BEGIN SELECT RAISE(ABORT,'immutable model observation'); END;
PRAGMA user_version=34;
