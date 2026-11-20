-- Owner consent for one exact audio version, invocation and observed Worker session.
CREATE TABLE transcription_input_grants (
 owner_id TEXT NOT NULL REFERENCES owners(owner_id),
 grant_id TEXT NOT NULL,
 author_id TEXT NOT NULL REFERENCES users(user_id),
 root_budget_id TEXT NOT NULL REFERENCES budget_roots(root_budget_id),
 invocation_id TEXT NOT NULL,
 upload_id TEXT NOT NULL,
 artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
 version_id TEXT NOT NULL,
 observation_id TEXT NOT NULL REFERENCES worker_transcription_profile_observations(observation_id),
 region_id TEXT NOT NULL,
 language TEXT,
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 projection_digest TEXT NOT NULL CHECK(projection_digest ~ '^[0-9a-f]{64}$'),
 measurement JSONB NOT NULL,
 chunk_digests TEXT[] NOT NULL CHECK(cardinality(chunk_digests) BETWEEN 1 AND 3052),
 organization_policy BIGINT, acl_version BIGINT,
 expires_at TIMESTAMPTZ NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(owner_id,grant_id),
 UNIQUE(owner_id,invocation_id),
 CHECK((organization_policy IS NULL)=(acl_version IS NULL)),
 CHECK(expires_at>created_at)
);
CREATE TRIGGER transcription_input_grant_frozen BEFORE UPDATE OR DELETE ON transcription_input_grants
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE transcription_input_revocations (
 owner_id TEXT NOT NULL REFERENCES owners(owner_id),grant_id TEXT NOT NULL,
 revoked_by TEXT NOT NULL REFERENCES users(user_id),revoked_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(owner_id,grant_id,revoked_by)
);
CREATE TRIGGER transcription_input_revocation_frozen BEFORE UPDATE OR DELETE ON transcription_input_revocations
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
