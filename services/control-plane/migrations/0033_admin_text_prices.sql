-- A draft never belongs to the quotable catalog. The user API only consumes
-- text_prices rows after an independently approved release becomes effective.
CREATE TABLE admin_text_price_drafts (
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
 meter_profile_digest text NOT NULL REFERENCES trusted_meter_profiles(profile_digest),
 input_price bigint NOT NULL CHECK(input_price>=0),
 output_price bigint NOT NULL CHECK(output_price>=0),
 output_limit integer NOT NULL CHECK(output_limit BETWEEN 1 AND 4096),
 fee_bps integer NOT NULL CHECK(fee_bps BETWEEN 0 AND 10000),
 terms_digest text NOT NULL CHECK(terms_digest ~ '^[0-9a-f]{64}$'),
 review_report_id text NOT NULL UNIQUE,
 created_at bigint NOT NULL,
 UNIQUE(created_by,request_key)
);
CREATE TRIGGER admin_price_draft_immutable BEFORE UPDATE OR DELETE ON admin_text_price_drafts FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE INDEX admin_price_draft_scope ON admin_text_price_drafts(scope_kind,scope_id,draft_sequence DESC);

CREATE TABLE admin_text_price_publications (
 price_version text PRIMARY KEY REFERENCES admin_text_price_drafts(price_version),
 action_id text NOT NULL UNIQUE REFERENCES admin_action_control(action_id),
 checker_id text NOT NULL REFERENCES admin_principals(principal_id),
 executor_id text NOT NULL REFERENCES admin_principals(principal_id),
 effective_at bigint NOT NULL,
 state text NOT NULL CHECK(state IN ('scheduled','active','blocked')),
 completed_at bigint,
 CHECK ((state='scheduled')=(completed_at IS NULL))
);
CREATE INDEX admin_price_due ON admin_text_price_publications(effective_at,price_version) WHERE state='scheduled';
CREATE FUNCTION protect_admin_price_publication() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR OLD.state!='scheduled' OR NEW.state='scheduled' OR
   (to_jsonb(OLD)-'state'-'completed_at') IS DISTINCT FROM (to_jsonb(NEW)-'state'-'completed_at')
 THEN RAISE EXCEPTION 'immutable price publication'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER admin_price_publication_mutation BEFORE UPDATE OR DELETE ON admin_text_price_publications FOR EACH ROW EXECUTE FUNCTION protect_admin_price_publication();
