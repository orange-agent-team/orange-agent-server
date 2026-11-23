-- A withdrawal may arrive before the registration it cancels. Keep that
-- durable intent scoped to the caller, even when no verifier was issued.
CREATE TABLE personal_automation_keys (
    user_id text NOT NULL REFERENCES users,
    key_id uuid NOT NULL,
    owner_id text NOT NULL REFERENCES owners,
    verifier text UNIQUE CHECK (verifier ~ '^[0-9a-f]{64}$'),
    scope text CHECK (scope IN ('wallet:read','budgets:read')),
    label text CHECK (length(label) BETWEEN 1 AND 64),
    ttl_seconds integer CHECK (ttl_seconds BETWEEN 60 AND 2592000),
    created_unix bigint NOT NULL DEFAULT floor(extract(epoch FROM clock_timestamp())),
    expires_unix bigint,
    revoked_unix bigint,
    PRIMARY KEY (user_id,key_id),
    CHECK ((verifier IS NULL AND revoked_unix IS NOT NULL AND scope IS NULL AND label IS NULL AND ttl_seconds IS NULL AND expires_unix IS NULL)
        OR (verifier IS NOT NULL AND scope IS NOT NULL AND label IS NOT NULL AND ttl_seconds IS NOT NULL AND expires_unix IS NOT NULL AND expires_unix=created_unix+ttl_seconds))
);
CREATE INDEX personal_automation_keys_owner ON personal_automation_keys(owner_id) WHERE revoked_unix IS NULL;

CREATE FUNCTION protect_personal_key() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN RAISE EXCEPTION 'key identity is retained'; END IF;
    IF (to_jsonb(NEW)-'revoked_unix') IS DISTINCT FROM (to_jsonb(OLD)-'revoked_unix')
        OR (OLD.revoked_unix IS NOT NULL AND NEW.revoked_unix IS DISTINCT FROM OLD.revoked_unix) THEN
        RAISE EXCEPTION 'immutable credential identity or revocation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER immutable_personal_automation_key BEFORE UPDATE OR DELETE ON personal_automation_keys
FOR EACH ROW EXECUTE FUNCTION protect_personal_key();

CREATE FUNCTION revoke_disabled_personal_keys() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME = 'users' THEN
        UPDATE personal_automation_keys SET revoked_unix=floor(extract(epoch FROM clock_timestamp()))
        WHERE user_id=OLD.user_id AND revoked_unix IS NULL;
    ELSE
        UPDATE personal_automation_keys SET revoked_unix=floor(extract(epoch FROM clock_timestamp()))
        WHERE owner_id=OLD.owner_id AND revoked_unix IS NULL;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER revoke_personal_keys_on_user_change AFTER UPDATE ON users FOR EACH ROW
WHEN ((OLD.active AND NOT NEW.active) OR OLD.personal_owner_id IS DISTINCT FROM NEW.personal_owner_id)
EXECUTE FUNCTION revoke_disabled_personal_keys();
CREATE TRIGGER revoke_personal_keys_on_owner_change AFTER UPDATE ON owners FOR EACH ROW
WHEN (OLD.state='active' AND NEW.state<>'active') EXECUTE FUNCTION revoke_disabled_personal_keys();
