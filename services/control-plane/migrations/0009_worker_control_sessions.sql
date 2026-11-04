-- Device certificates are provisioned by a trusted enrollment signer, never by Hello.
CREATE TABLE worker_credentials(
 certificate_sha256 TEXT PRIMARY KEY CHECK(certificate_sha256 ~ '^[0-9a-f]{64}$'),
 node_id TEXT NOT NULL REFERENCES nodes(node_id),
 enrollment_user_id TEXT NOT NULL REFERENCES users(user_id),
 active BOOLEAN NOT NULL DEFAULT TRUE,
 not_before TIMESTAMPTZ NOT NULL,expires_at TIMESTAMPTZ NOT NULL,
 CHECK(expires_at>not_before)
);
CREATE TABLE worker_sessions(
 node_id TEXT PRIMARY KEY REFERENCES nodes(node_id),
 gateway_instance_id TEXT NOT NULL,connection_epoch BIGINT NOT NULL CHECK(connection_epoch>0),
 session_id TEXT NOT NULL UNIQUE,
 certificate_sha256 TEXT NOT NULL REFERENCES worker_credentials(certificate_sha256),
 capabilities_digest TEXT NOT NULL CHECK(capabilities_digest ~ '^[0-9a-f]{64}$'),
 reported_capability_snapshot_id TEXT,
 last_seen_at TIMESTAMPTZ NOT NULL,expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE worker_session_audit(
 event_id TEXT PRIMARY KEY,node_id TEXT NOT NULL REFERENCES nodes(node_id),
 gateway_instance_id TEXT NOT NULL,connection_epoch BIGINT NOT NULL,
 session_id TEXT NOT NULL,kind TEXT NOT NULL,
 occurred_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER worker_session_audit_immutable BEFORE UPDATE OR DELETE ON worker_session_audit
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE INDEX worker_credentials_node ON worker_credentials(node_id);
