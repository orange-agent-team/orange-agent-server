CREATE TABLE event_streams (
  stream_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
  owner_id TEXT NOT NULL REFERENCES owners(owner_id),
  entity_kind TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  next_seq BIGINT NOT NULL DEFAULT 1 CHECK(next_seq>0),
  oldest_seq BIGINT NOT NULL DEFAULT 1 CHECK(oldest_seq>0 AND oldest_seq<=next_seq),
  UNIQUE(owner_id,entity_kind,entity_id)
);
CREATE TABLE durable_events (
  event_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
  stream_id TEXT NOT NULL REFERENCES event_streams(stream_id),
  seq BIGINT NOT NULL CHECK(seq>0),
  event_type TEXT NOT NULL,
  payload_digest TEXT NOT NULL CHECK(payload_digest ~ '^[a-f0-9]{64}$'),
  correlation_id TEXT NOT NULL,
  causation_id TEXT,
  occurred_at TIMESTAMPTZ NOT NULL,
  source_outbox_id BIGINT NOT NULL UNIQUE,
  UNIQUE(stream_id,seq)
);
CREATE TRIGGER durable_event_immutable BEFORE UPDATE ON durable_events
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
ALTER TABLE outbox ADD COLUMN domain_event_id TEXT UNIQUE REFERENCES durable_events(event_id);
-- Preserve the existing committed development event history; never invent missing payload bodies.
INSERT INTO event_streams(owner_id,entity_kind,entity_id)
SELECT DISTINCT owner_id,split_part(event_type,'.',1),entity_id FROM outbox;
INSERT INTO durable_events(stream_id,seq,event_type,payload_digest,correlation_id,occurred_at,source_outbox_id)
SELECT s.stream_id,row_number() OVER(PARTITION BY s.stream_id ORDER BY o.event_id),o.event_type,encode(sha256(convert_to(o.payload,'UTF8')),'hex'),o.entity_id,o.created_at,o.event_id
FROM outbox o JOIN event_streams s ON s.owner_id=o.owner_id AND s.entity_kind=split_part(o.event_type,'.',1) AND s.entity_id=o.entity_id;
UPDATE event_streams s SET next_seq=(SELECT max(e.seq)+1 FROM durable_events e WHERE e.stream_id=s.stream_id);
UPDATE outbox o SET domain_event_id=e.event_id FROM durable_events e WHERE e.source_outbox_id=o.event_id;
ALTER TABLE outbox ALTER COLUMN domain_event_id SET NOT NULL;
-- Future writes insert the durable event first, then its outbox row with the reserved sequence ID.
