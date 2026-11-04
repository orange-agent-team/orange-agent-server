CREATE TABLE nodes (
  node_id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL REFERENCES owners(owner_id),
  environment TEXT NOT NULL CHECK(environment IN ('dev','staging','production')),
  region_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('pending','online','offline','draining','quarantined','revoked')),
  health_report_id TEXT,
  version BIGINT NOT NULL DEFAULT 1 CHECK(version>0)
);
CREATE TABLE reconciliation_cases (
  case_id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL REFERENCES owners(owner_id),
  environment TEXT NOT NULL CHECK(environment IN ('dev','staging','production')),
  business_ref TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('open','investigating','waiting_external','resolved')),
  next_check_at BIGINT NOT NULL,
  version BIGINT NOT NULL DEFAULT 1 CHECK(version>0)
);
