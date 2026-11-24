-- Issued PUTs have a fixed deadline, stable staging key and original write-policy stamp.
CREATE TABLE artifact_s3_upload_grants(
    transfer_id text PRIMARY KEY REFERENCES artifact_transfers(transfer_id),
    owner_id text NOT NULL,
    upload_id text NOT NULL,
    requester_id text NOT NULL REFERENCES users(user_id),
    request_key text NOT NULL,
    organization_version bigint,
    acl_version bigint,
    expires_unix bigint NOT NULL,
    UNIQUE(owner_id,requester_id,request_key),
    FOREIGN KEY(owner_id,upload_id) REFERENCES artifact_uploads(owner_id,upload_id)
);
CREATE TRIGGER immutable_s3_upload_grants BEFORE UPDATE OR DELETE ON artifact_s3_upload_grants
    FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE artifact_s3_confirmations(
    transfer_id text PRIMARY KEY REFERENCES artifact_s3_upload_grants(transfer_id),
    source_version text NOT NULL CHECK(source_version<>'null' AND octet_length(source_version) BETWEEN 1 AND 1024)
);
CREATE TRIGGER immutable_s3_confirmation BEFORE UPDATE OR DELETE ON artifact_s3_confirmations
    FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
