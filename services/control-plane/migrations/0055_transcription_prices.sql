CREATE TABLE transcription_prices(
 price_version TEXT PRIMARY KEY,
 node_id TEXT NOT NULL REFERENCES nodes(node_id),
 execution_profile_id TEXT NOT NULL REFERENCES reviewed_execution_profiles(execution_profile_id),
 price_per_minute BIGINT NOT NULL CHECK(price_per_minute>0),
 fee_bps INTEGER NOT NULL CHECK(fee_bps BETWEEN 0 AND 10000),
 review_digest TEXT NOT NULL CHECK(review_digest ~ '^[a-f0-9]{64}$'),
 admission TEXT NOT NULL CHECK(admission='development_only'),
 enabled BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE UNIQUE INDEX one_active_transcription_price ON transcription_prices(node_id,execution_profile_id) WHERE enabled;
CREATE FUNCTION protect_transcription_price() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'immutable price version'; END IF;
 IF (to_jsonb(NEW)-'enabled') IS DISTINCT FROM (to_jsonb(OLD)-'enabled') THEN RAISE EXCEPTION 'immutable price terms'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER transcription_price_version BEFORE UPDATE OR DELETE ON transcription_prices FOR EACH ROW EXECUTE FUNCTION protect_transcription_price();
CREATE TABLE transcription_quote_terms (
 quote_id TEXT PRIMARY KEY REFERENCES quotes(quote_id),
 owner_id TEXT NOT NULL, grant_id TEXT NOT NULL, requester_id TEXT NOT NULL REFERENCES users(user_id),
 request_id TEXT NOT NULL, request_digest TEXT NOT NULL CHECK(request_digest ~ '^[a-f0-9]{64}$'),
 price_version TEXT NOT NULL REFERENCES transcription_prices(price_version),provider_owner_id TEXT NOT NULL REFERENCES owners(owner_id),
 duration_ms BIGINT NOT NULL CHECK(duration_ms BETWEEN 1 AND 600000),
 price_per_minute BIGINT NOT NULL CHECK(price_per_minute>0),fee_bps INTEGER NOT NULL CHECK(fee_bps BETWEEN 0 AND 10000),
 observation_id TEXT NOT NULL REFERENCES worker_transcription_profile_observations(observation_id),
 UNIQUE(owner_id,request_id),FOREIGN KEY(owner_id,grant_id) REFERENCES transcription_input_grants(owner_id,grant_id)
);
CREATE TRIGGER frozen_transcription_quote BEFORE UPDATE OR DELETE ON transcription_quote_terms FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
-- This also rejects an older binary attempting to use an audio quote as a text quote. Its transaction rolls back the reserve.
CREATE FUNCTION bind_transcription_quote_purpose() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM transcription_quote_terms t WHERE t.quote_id=NEW.quote_id) AND NOT EXISTS(
 SELECT 1 FROM transcription_quote_terms t JOIN transcription_input_grants g USING(owner_id,grant_id)
 WHERE t.quote_id=NEW.quote_id AND NEW.purpose='transcription' AND NEW.input_required AND NEW.submitting_user_id=t.requester_id
 AND NEW.owner_id=g.owner_id AND NEW.root_budget_id=g.root_budget_id AND NEW.invocation_id=g.invocation_id) THEN
 RAISE EXCEPTION 'transcription quote can only fund its consenting actor and invocation'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER job_transcription_quote BEFORE INSERT ON jobs FOR EACH ROW EXECUTE FUNCTION bind_transcription_quote_purpose();

-- Preserve the invariant that a settlement requires a measured committed delivery for either modality.
CREATE TABLE metered_delivery_ids(
 job_id TEXT PRIMARY KEY REFERENCES job_response_commits(job_id),modality TEXT NOT NULL CHECK(modality IN ('text','transcription'))
);
INSERT INTO metered_delivery_ids(job_id,modality) SELECT job_id,'text' FROM job_usage_facts;
CREATE FUNCTION link_metered_delivery() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_TABLE_NAME='job_usage_facts' THEN INSERT INTO metered_delivery_ids VALUES(NEW.job_id,'text');
 ELSIF EXISTS(SELECT 1 FROM jobs j JOIN transcription_quote_terms t USING(quote_id) WHERE j.job_id=NEW.job_id) THEN
 INSERT INTO metered_delivery_ids VALUES(NEW.job_id,'transcription'); END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER text_metered_delivery AFTER INSERT ON job_usage_facts FOR EACH ROW EXECUTE FUNCTION link_metered_delivery();
CREATE TRIGGER asr_metered_delivery AFTER INSERT ON transcription_response_facts FOR EACH ROW EXECUTE FUNCTION link_metered_delivery();
CREATE TRIGGER immutable_metered_delivery BEFORE UPDATE OR DELETE ON metered_delivery_ids FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
ALTER TABLE job_settlements DROP CONSTRAINT job_settlements_job_id_fkey,
 ADD FOREIGN KEY(job_id) REFERENCES metered_delivery_ids(job_id);

CREATE VIEW current_metered_deliveries AS
SELECT j.job_id,j.owner_id,j.root_budget_id,m.provider_owner_id,d.modality,c.committed_at
FROM metered_delivery_ids d JOIN jobs j USING(job_id) JOIN job_response_commits c USING(job_id) JOIN quote_metering m USING(quote_id) WHERE d.modality='text'
UNION ALL SELECT j.job_id,j.owner_id,j.root_budget_id,m.provider_owner_id,d.modality,c.committed_at
FROM metered_delivery_ids d JOIN jobs j USING(job_id) JOIN job_response_commits c USING(job_id) JOIN transcription_quote_terms m USING(quote_id) WHERE d.modality='transcription';
