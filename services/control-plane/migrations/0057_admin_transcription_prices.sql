CREATE TABLE admin_transcription_price_drafts (
 draft_sequence bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
 price_version text PRIMARY KEY,
 created_by text NOT NULL REFERENCES admin_principals(principal_id),
 request_key text NOT NULL,
 request_digest text NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 scope_kind text NOT NULL CHECK(scope_kind IN ('platform','tenant')),
 scope_id text NOT NULL,
 provider_owner_id text NOT NULL REFERENCES owners(owner_id),
 node_id text NOT NULL REFERENCES nodes(node_id),
 execution_profile_id text NOT NULL REFERENCES reviewed_execution_profiles(execution_profile_id),
 observation_id text NOT NULL REFERENCES transcription_process_identities(observation_id),
 price_per_minute bigint NOT NULL CHECK(price_per_minute>0),
 fee_bps integer NOT NULL CHECK(fee_bps BETWEEN 0 AND 10000),
 terms_digest text NOT NULL CHECK(terms_digest ~ '^[0-9a-f]{64}$'),
 review_report_id text NOT NULL UNIQUE,
 created_at bigint NOT NULL,
 UNIQUE(created_by,request_key)
);
CREATE TRIGGER admin_asr_draft_immutable BEFORE UPDATE OR DELETE ON admin_transcription_price_drafts FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE INDEX admin_asr_draft_scope ON admin_transcription_price_drafts(scope_kind,scope_id,draft_sequence DESC);

CREATE TABLE admin_transcription_price_publications (
 price_version text PRIMARY KEY REFERENCES admin_transcription_price_drafts(price_version),
 action_id text NOT NULL UNIQUE REFERENCES admin_action_control(action_id),
 checker_id text NOT NULL REFERENCES admin_principals(principal_id),
 executor_id text NOT NULL REFERENCES admin_principals(principal_id),
 effective_at bigint NOT NULL,
 state text NOT NULL CHECK(state IN ('scheduled','active','blocked')),
 completed_at bigint,
 CHECK ((state='scheduled')=(completed_at IS NULL))
);
CREATE INDEX admin_asr_price_due ON admin_transcription_price_publications(effective_at,price_version) WHERE state='scheduled';
CREATE TRIGGER immutable_asr_publication BEFORE UPDATE OR DELETE ON admin_transcription_price_publications
FOR EACH ROW EXECUTE FUNCTION protect_admin_price_publication();
