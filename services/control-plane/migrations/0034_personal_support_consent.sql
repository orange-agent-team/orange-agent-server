-- A user's consent is a prerequisite for a separate operator grant. Neither
-- table grants an administrator direct access to artifact storage.
CREATE TABLE personal_support_consents (
  authorization_id text PRIMARY KEY,
  sequence bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  user_id text NOT NULL REFERENCES users(user_id),
  owner_id text NOT NULL REFERENCES owners(owner_id),
  request_key text NOT NULL,
  request_digest text NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
  ticket_id text NOT NULL,
  purpose text NOT NULL CHECK(length(purpose) BETWEEN 1 AND 500),
  resource_versions jsonb NOT NULL CHECK(jsonb_typeof(resource_versions)='array' AND jsonb_array_length(resource_versions) BETWEEN 1 AND 20),
  content_field text NOT NULL CHECK(content_field='file_content'),
  created_at bigint NOT NULL,
  expires_at bigint NOT NULL CHECK(expires_at>created_at AND expires_at<=created_at+86400),
  UNIQUE (user_id,request_key)
);
CREATE INDEX personal_support_directory ON personal_support_consents(user_id,sequence DESC);
CREATE TRIGGER personal_support_consent_immutable BEFORE UPDATE OR DELETE ON personal_support_consents
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE personal_support_revocations (
  authorization_id text PRIMARY KEY REFERENCES personal_support_consents(authorization_id),
  revoked_at bigint NOT NULL
);
CREATE TRIGGER personal_support_revocation_immutable BEFORE UPDATE OR DELETE ON personal_support_revocations
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
