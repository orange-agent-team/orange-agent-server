-- Platform-managed hosts use a distinct identity registry and execution domain.
-- Public worker certificates, compute leases and model payments are separate.
CREATE TABLE cloud_pool_hosts (
  host_instance_id text PRIMARY KEY,
  certificate_sha256 text NOT NULL UNIQUE CHECK(certificate_sha256 ~ '^[0-9a-f]{64}$'),
  profile_id text NOT NULL CHECK(profile_id='dev-gvisor-linux-arm64-v1'),
  attestation_digest text NOT NULL CHECK(attestation_digest ~ '^[0-9a-f]{64}$'),
  region_id text NOT NULL,
  max_workspaces integer NOT NULL CHECK(max_workspaces BETWEEN 1 AND 8),
  enabled boolean NOT NULL DEFAULT true,
  credential_expires_at timestamptz NOT NULL,
  accepting boolean NOT NULL DEFAULT false,
  live_until timestamptz
);
CREATE TABLE cloud_workspace_quotas (
  owner_id text PRIMARY KEY REFERENCES owners(owner_id),
  max_active integer NOT NULL DEFAULT 2 CHECK(max_active BETWEEN 1 AND 8)
);
CREATE TABLE cloud_runs (
  run_id text PRIMARY KEY,
  owner_id text NOT NULL REFERENCES owners(owner_id),user_id text NOT NULL REFERENCES users(user_id),
  request_key text NOT NULL,request_digest text NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
  root_budget_id text NOT NULL REFERENCES budget_roots(root_budget_id),
  input_artifact_id text NOT NULL,input_version_id text NOT NULL,
  FOREIGN KEY(input_artifact_id,input_version_id) REFERENCES artifacts(artifact_id,version_id),
  status text NOT NULL CHECK(status IN ('queued','running','cancelled','recovery_required','failed','completed','waiting_approval','paused')),
  version bigint NOT NULL DEFAULT 1 CHECK(version>=1),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  sequence bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  UNIQUE(owner_id,request_key)
);
CREATE INDEX cloud_runs_owner_sequence ON cloud_runs(owner_id,sequence DESC);
CREATE TABLE cloud_workspaces (
  workspace_id text PRIMARY KEY,run_id text NOT NULL UNIQUE REFERENCES cloud_runs(run_id),
  owner_id text NOT NULL REFERENCES owners(owner_id),host_instance_id text NOT NULL REFERENCES cloud_pool_hosts(host_instance_id),
  profile_id text NOT NULL CHECK(profile_id='dev-gvisor-linux-arm64-v1'),
  status text NOT NULL CHECK(status IN ('requested','preparing','ready','running','draining','deleted','quarantined')),
  version bigint NOT NULL DEFAULT 1 CHECK(version>=1),epoch bigint NOT NULL DEFAULT 0 CHECK(epoch>=0),
  expires_at timestamptz NOT NULL,lease_until timestamptz,
  CHECK(status!='requested' OR (epoch=0 AND lease_until IS NULL))
);
CREATE INDEX cloud_workspaces_owner_capacity ON cloud_workspaces(owner_id) WHERE status!='deleted';
CREATE INDEX cloud_workspaces_host_capacity ON cloud_workspaces(host_instance_id) WHERE status!='deleted';
CREATE TABLE cloud_run_events (
  sequence bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  run_id text NOT NULL REFERENCES cloud_runs(run_id), run_version bigint NOT NULL CHECK(run_version>0),
  kind text NOT NULL,occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  UNIQUE(run_id,run_version)
);
CREATE TRIGGER cloud_run_events_immutable BEFORE UPDATE OR DELETE ON cloud_run_events
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE cloud_workspace_claims (
  allocation_id text PRIMARY KEY,
  workspace_id text NOT NULL UNIQUE REFERENCES cloud_workspaces(workspace_id),
  host_instance_id text NOT NULL REFERENCES cloud_pool_hosts(host_instance_id),
  command_id text NOT NULL,request_digest text NOT NULL,epoch bigint NOT NULL CHECK(epoch>0),
  initial_until bigint NOT NULL, UNIQUE(host_instance_id,command_id)
);
CREATE TRIGGER cloud_claim_immutable BEFORE UPDATE OR DELETE ON cloud_workspace_claims
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
ALTER TABLE cloud_workspaces ADD COLUMN previous_lease_until bigint;
CREATE TABLE cloud_workspace_cleanup_receipts (
  workspace_id text PRIMARY KEY REFERENCES cloud_workspaces(workspace_id),
  allocation_id text NOT NULL UNIQUE REFERENCES cloud_workspace_claims(allocation_id),
  evidence_digest text NOT NULL CHECK(evidence_digest ~ '^[0-9a-f]{64}$'),
  receipt_id text NOT NULL UNIQUE,
  recorded_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_workspace_cleanup_immutable BEFORE UPDATE OR DELETE ON cloud_workspace_cleanup_receipts
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
ALTER TABLE cloud_runs ADD UNIQUE(run_id,owner_id);
ALTER TABLE cloud_workspaces ADD FOREIGN KEY(run_id,owner_id) REFERENCES cloud_runs(run_id,owner_id);
CREATE FUNCTION protect_cloud_host_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP='DELETE' OR (NOT OLD.enabled AND NEW.enabled)
     OR (to_jsonb(OLD)-'enabled'-'live_until'-'accepting') IS DISTINCT FROM
        (to_jsonb(NEW)-'enabled'-'live_until'-'accepting') THEN
    RAISE EXCEPTION 'retired platform host identity cannot be reused';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER cloud_host_identity BEFORE UPDATE OR DELETE ON cloud_pool_hosts
  FOR EACH ROW EXECUTE FUNCTION protect_cloud_host_identity();
CREATE TABLE cloud_run_withdrawals(
 owner_id text NOT NULL REFERENCES owners(owner_id),user_id text NOT NULL REFERENCES users(user_id),
 request_key text NOT NULL,receipt_id text NOT NULL UNIQUE,
 occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),PRIMARY KEY(owner_id,request_key)
);
CREATE TRIGGER cloud_withdrawal_immutable BEFORE UPDATE OR DELETE ON cloud_run_withdrawals
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE FUNCTION protect_cloud_run_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP='DELETE' OR (to_jsonb(OLD)-'version'-'status') IS DISTINCT FROM (to_jsonb(NEW)-'version'-'status')
     OR NEW.version<>OLD.version+1 THEN RAISE EXCEPTION 'immutable cloud run identity or invalid version'; END IF;
  IF OLD.status IN ('cancelled','completed','failed') AND NEW.status<>OLD.status THEN
    RAISE EXCEPTION 'terminal cloud run cannot be reopened'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER cloud_run_identity BEFORE UPDATE OR DELETE ON cloud_runs
  FOR EACH ROW EXECUTE FUNCTION protect_cloud_run_identity();

CREATE FUNCTION protect_cloud_workspace_state() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP='DELETE' OR OLD.status='deleted' OR NEW.expires_at>OLD.expires_at OR NEW.epoch<OLD.epoch OR
     ROW(NEW.workspace_id,NEW.run_id,NEW.owner_id,NEW.host_instance_id,NEW.profile_id) IS DISTINCT FROM
     ROW(OLD.workspace_id,OLD.run_id,OLD.owner_id,OLD.host_instance_id,OLD.profile_id) THEN
    RAISE EXCEPTION 'invalid workspace identity or lifetime'; END IF;
  IF NEW.status=OLD.status THEN
    IF NEW.version NOT IN (OLD.version,OLD.version+1) OR NEW.epoch<>OLD.epoch THEN RAISE EXCEPTION 'invalid workspace revision'; END IF;
    RETURN NEW;
  END IF;
  IF NEW.version<>OLD.version+1 OR NOT (
    (OLD.status='requested' AND NEW.status IN ('preparing','deleted')) OR
    (OLD.status='preparing' AND NEW.status IN ('ready','draining','quarantined')) OR
    (OLD.status='ready' AND NEW.status IN ('running','draining','quarantined')) OR
    (OLD.status='running' AND NEW.status IN ('ready','draining','quarantined')) OR
    (OLD.status='draining' AND NEW.status IN ('deleted','quarantined')) OR
    (OLD.status='quarantined' AND NEW.status='draining')) THEN RAISE EXCEPTION 'invalid workspace transition'; END IF;
  IF NEW.status='deleted' AND NOT (
    (OLD.status='requested' AND OLD.epoch=0 AND NOT EXISTS(SELECT 1 FROM cloud_workspace_claims WHERE workspace_id=OLD.workspace_id)) OR
    (OLD.status='draining' AND EXISTS(SELECT 1 FROM cloud_workspace_cleanup_receipts WHERE workspace_id=OLD.workspace_id))
  ) THEN RAISE EXCEPTION 'workspace cleanup proof required'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER cloud_workspace_state BEFORE UPDATE OR DELETE ON cloud_workspaces
  FOR EACH ROW EXECUTE FUNCTION protect_cloud_workspace_state();
