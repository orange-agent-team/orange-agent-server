ALTER TABLE cloud_pool_hosts ADD COLUMN input_protocol_version int NOT NULL DEFAULT 0 CHECK(input_protocol_version IN (0,1));
-- Protocol negotiation is liveness metadata. Registration and retirement remain immutable.
CREATE OR REPLACE FUNCTION protect_cloud_host_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP='DELETE' OR (NOT OLD.enabled AND NEW.enabled)
     OR (to_jsonb(OLD)-'enabled'-'live_until'-'accepting'-'input_protocol_version') IS DISTINCT FROM
        (to_jsonb(NEW)-'enabled'-'live_until'-'accepting'-'input_protocol_version') THEN
    RAISE EXCEPTION 'retired platform host identity cannot be reused';
  END IF;
  RETURN NEW;
END $$;
CREATE TABLE cloud_workspace_input_reads (
 allocation_id text PRIMARY KEY REFERENCES cloud_workspace_claims(allocation_id),receipt_id text NOT NULL UNIQUE,
 artifact_id text NOT NULL,version_id text NOT NULL,sha256 text NOT NULL CHECK(sha256 ~ '^[0-9a-f]{64}$'),
 FOREIGN KEY(artifact_id,version_id) REFERENCES artifacts(artifact_id,version_id),
 recorded_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_workspace_input_immutable BEFORE UPDATE OR DELETE ON cloud_workspace_input_reads
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_workspace_ready_receipts (
 allocation_id text PRIMARY KEY REFERENCES cloud_workspace_claims(allocation_id),receipt_id text NOT NULL UNIQUE,
 input_receipt_id text NOT NULL UNIQUE REFERENCES cloud_workspace_input_reads(receipt_id),
 sandbox_id text NOT NULL UNIQUE,request_digest text NOT NULL,materialization_digest text NOT NULL,
 recorded_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_readiness_immutable BEFORE UPDATE OR DELETE ON cloud_workspace_ready_receipts
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
-- The phase transition is possible only after a matching admitted input and
-- materialization receipt have committed in the same transaction.
CREATE FUNCTION require_cloud_readiness_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS(SELECT 1 FROM cloud_workspace_claims c JOIN cloud_workspace_ready_receipts r USING(allocation_id)
    WHERE c.workspace_id=NEW.workspace_id AND c.epoch=NEW.epoch) THEN RAISE EXCEPTION 'no workspace readiness evidence';END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER cloud_readiness_evidence BEFORE UPDATE OF status ON cloud_workspaces FOR EACH ROW
 WHEN (NEW.status='ready' AND OLD.status='preparing') EXECUTE FUNCTION require_cloud_readiness_receipt();
