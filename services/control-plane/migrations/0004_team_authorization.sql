-- Organization roles and resource ACLs are independent. No role implies private-content access.
CREATE TABLE users (
  user_id TEXT PRIMARY KEY,
  issuer TEXT NOT NULL,
  subject TEXT NOT NULL,
  personal_owner_id TEXT NOT NULL UNIQUE REFERENCES owners(owner_id),
  active BOOLEAN NOT NULL DEFAULT TRUE,
  UNIQUE(issuer,subject)
);
CREATE TABLE organizations (
  organization_id TEXT PRIMARY KEY REFERENCES owners(owner_id),
  name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 100),
  member_limit INTEGER NOT NULL DEFAULT 20 CHECK(member_limit BETWEEN 1 AND 100),
  policy_version BIGINT NOT NULL DEFAULT 1 CHECK(policy_version>0)
);
CREATE TABLE memberships (
  organization_id TEXT NOT NULL REFERENCES organizations(organization_id),
  user_id TEXT NOT NULL REFERENCES users(user_id),
  role TEXT NOT NULL CHECK(role IN ('owner','admin','member','billing','viewer')),
  active BOOLEAN NOT NULL DEFAULT TRUE,
  PRIMARY KEY(organization_id,user_id)
);
CREATE TABLE team_resources (
  resource_id TEXT PRIMARY KEY,
  organization_id TEXT NOT NULL REFERENCES organizations(organization_id),
  creator_id TEXT NOT NULL REFERENCES users(user_id),
  kind TEXT NOT NULL CHECK(kind IN ('thread','run','artifact','memory','skill')),
  deleted BOOLEAN NOT NULL DEFAULT FALSE,
  acl_version BIGINT NOT NULL DEFAULT 1 CHECK(acl_version>0)
);
CREATE TABLE resource_acl (
  resource_id TEXT NOT NULL REFERENCES team_resources(resource_id),
  user_id TEXT NOT NULL REFERENCES users(user_id),
  access TEXT NOT NULL CHECK(access IN ('read','write')),
  revoked BOOLEAN NOT NULL DEFAULT FALSE,
  PRIMARY KEY(resource_id,user_id)
);
CREATE TABLE team_request_keys (
  actor_id TEXT NOT NULL REFERENCES users(user_id),
  scope TEXT NOT NULL,
  operation TEXT NOT NULL,
  key TEXT NOT NULL,
  digest TEXT NOT NULL,
  result JSONB NOT NULL,
  PRIMARY KEY(actor_id,scope,operation,key)
);
CREATE TABLE team_audit (
  event_id TEXT PRIMARY KEY,
  organization_id TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  target_id TEXT NOT NULL,
  payload_digest TEXT NOT NULL,
  occurred_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER team_audit_immutable BEFORE UPDATE OR DELETE ON team_audit
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
