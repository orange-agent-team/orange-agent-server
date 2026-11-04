CREATE TABLE queue_subscriptions (
  consumer_id TEXT NOT NULL,
  owner_id TEXT NOT NULL REFERENCES owners(owner_id),
  event_type TEXT NOT NULL,
  active BOOLEAN NOT NULL DEFAULT TRUE,
  version BIGINT NOT NULL DEFAULT 1 CHECK(version>0),
  PRIMARY KEY(consumer_id,owner_id,event_type)
);
CREATE TABLE inbox (
  consumer_id TEXT NOT NULL,
  event_id TEXT NOT NULL,
  owner_id TEXT NOT NULL REFERENCES owners(owner_id),
  event_type TEXT NOT NULL,
  envelope JSONB NOT NULL,
  payload TEXT NOT NULL,
  envelope_digest TEXT NOT NULL CHECK(envelope_digest ~ '^[a-f0-9]{64}$'),
  state TEXT NOT NULL DEFAULT 'ready' CHECK(state IN ('ready','leased','done','dead')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 3),
  available_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
  lease_id TEXT,
  lease_expires_at TIMESTAMPTZ,
  lease_subscription_version BIGINT,
  last_error TEXT,
  completed_at TIMESTAMPTZ,
  PRIMARY KEY(consumer_id,event_id),
  CHECK((state='leased' AND lease_id IS NOT NULL AND lease_expires_at IS NOT NULL AND lease_subscription_version IS NOT NULL)
     OR (state!='leased' AND lease_id IS NULL AND lease_expires_at IS NULL AND lease_subscription_version IS NULL))
);
CREATE INDEX inbox_ready ON inbox(consumer_id,state,available_at);
CREATE FUNCTION protect_inbox_message() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.consumer_id IS DISTINCT FROM OLD.consumer_id OR NEW.event_id IS DISTINCT FROM OLD.event_id
    OR NEW.owner_id IS DISTINCT FROM OLD.owner_id OR NEW.event_type IS DISTINCT FROM OLD.event_type
    OR NEW.envelope IS DISTINCT FROM OLD.envelope OR NEW.payload IS DISTINCT FROM OLD.payload
    OR NEW.envelope_digest IS DISTINCT FROM OLD.envelope_digest THEN
    RAISE EXCEPTION 'immutable inbox message' USING ERRCODE='23514';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER inbox_message_immutable BEFORE UPDATE ON inbox FOR EACH ROW EXECUTE FUNCTION protect_inbox_message();
CREATE TABLE queue_dead_letters (
  consumer_id TEXT NOT NULL,
  event_id TEXT NOT NULL,
  reason TEXT NOT NULL,
  attempts INTEGER NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY(consumer_id,event_id),
  FOREIGN KEY(consumer_id,event_id) REFERENCES inbox(consumer_id,event_id)
);
CREATE TRIGGER dead_letter_immutable BEFORE UPDATE OR DELETE ON queue_dead_letters
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE FUNCTION protect_outbox_message() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.event_id IS DISTINCT FROM OLD.event_id OR NEW.owner_id IS DISTINCT FROM OLD.owner_id
    OR NEW.entity_id IS DISTINCT FROM OLD.entity_id OR NEW.event_type IS DISTINCT FROM OLD.event_type
    OR NEW.payload IS DISTINCT FROM OLD.payload OR NEW.domain_event_id IS DISTINCT FROM OLD.domain_event_id
    OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
    RAISE EXCEPTION 'immutable outbox message' USING ERRCODE='23514';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER outbox_message_immutable BEFORE UPDATE ON outbox FOR EACH ROW EXECUTE FUNCTION protect_outbox_message();
