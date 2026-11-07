-- Development admission only. No public mutation/payment endpoint is introduced.
CREATE TABLE trusted_meter_profiles(
 profile_digest TEXT PRIMARY KEY CHECK(profile_digest ~ '^[0-9a-f]{64}$'),
 descriptor JSONB NOT NULL, enabled BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE FUNCTION protect_meter_profile() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR NEW.profile_digest IS DISTINCT FROM OLD.profile_digest OR NEW.descriptor IS DISTINCT FROM OLD.descriptor THEN RAISE EXCEPTION 'immutable meter profile'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER meter_profile_immutable BEFORE UPDATE OR DELETE ON trusted_meter_profiles FOR EACH ROW EXECUTE FUNCTION protect_meter_profile();
CREATE TABLE text_prices(
 price_version TEXT PRIMARY KEY,
 node_id TEXT NOT NULL REFERENCES nodes(node_id),
 execution_profile_id TEXT NOT NULL REFERENCES reviewed_execution_profiles(execution_profile_id),
 meter_profile_digest TEXT NOT NULL REFERENCES trusted_meter_profiles(profile_digest),
 input_price BIGINT NOT NULL CHECK(input_price>=0), output_price BIGINT NOT NULL CHECK(output_price>=0),
 output_limit BIGINT NOT NULL CHECK(output_limit BETWEEN 1 AND 4096), fee_bps INTEGER NOT NULL CHECK(fee_bps BETWEEN 0 AND 10000),
 admission TEXT NOT NULL DEFAULT 'development_only' CHECK(admission='development_only'),enabled BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE FUNCTION protect_text_price() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR (to_jsonb(NEW)-'enabled') IS DISTINCT FROM (to_jsonb(OLD)-'enabled') THEN RAISE EXCEPTION 'immutable price version'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER text_price_immutable BEFORE UPDATE OR DELETE ON text_prices FOR EACH ROW EXECUTE FUNCTION protect_text_price();
CREATE TABLE quote_metering(
 quote_id TEXT PRIMARY KEY REFERENCES quotes(quote_id), request_id TEXT NOT NULL,
 owner_id TEXT NOT NULL REFERENCES owners(owner_id), requester_id TEXT NOT NULL REFERENCES users(user_id),request_digest TEXT NOT NULL,
 input_artifact_id TEXT NOT NULL, input_version_id TEXT NOT NULL,
 input_units BIGINT NOT NULL CHECK(input_units>=0),output_limit BIGINT NOT NULL CHECK(output_limit BETWEEN 1 AND 4096),
 price_version TEXT NOT NULL REFERENCES text_prices(price_version), meter_profile_digest TEXT NOT NULL REFERENCES trusted_meter_profiles(profile_digest),
 provider_owner_id TEXT NOT NULL REFERENCES owners(owner_id),
 input_price BIGINT NOT NULL CHECK(input_price>=0),output_price BIGINT NOT NULL CHECK(output_price>=0),fee_bps INTEGER NOT NULL CHECK(fee_bps BETWEEN 0 AND 10000),
 UNIQUE(owner_id,request_id),FOREIGN KEY(input_artifact_id,input_version_id) REFERENCES artifacts(artifact_id,version_id)
);
CREATE TRIGGER quote_metering_immutable BEFORE UPDATE OR DELETE ON quote_metering FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE job_usage_facts(
 job_id TEXT PRIMARY KEY REFERENCES job_response_commits(job_id),
 meter_profile_digest TEXT NOT NULL REFERENCES trusted_meter_profiles(profile_digest),
 request_projection_digest TEXT NOT NULL CHECK(request_projection_digest ~ '^[0-9a-f]{64}$'),
 response_digest TEXT NOT NULL CHECK(response_digest ~ '^[0-9a-f]{64}$'),
 output_projection_digest TEXT NOT NULL CHECK(output_projection_digest ~ '^[0-9a-f]{64}$'),
 input_units BIGINT NOT NULL CHECK(input_units>=0),output_units BIGINT NOT NULL CHECK(output_units>=0),has_tools BOOLEAN NOT NULL,
 measured_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER usage_fact_immutable BEFORE UPDATE OR DELETE ON job_usage_facts FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
ALTER TABLE wallets ADD COLUMN pending_earned BIGINT NOT NULL DEFAULT 0 CHECK(pending_earned>=0);
CREATE TABLE platform_credit_balances(account TEXT PRIMARY KEY,balance BIGINT NOT NULL DEFAULT 0 CHECK(balance>=0));
INSERT INTO platform_credit_balances(account) VALUES('fees');
CREATE TABLE job_settlements(
 job_id TEXT PRIMARY KEY REFERENCES job_usage_facts(job_id),transaction_id TEXT NOT NULL UNIQUE REFERENCES ledger_transactions(transaction_id),
 charge BIGINT NOT NULL CHECK(charge>=0),platform_fee BIGINT NOT NULL CHECK(platform_fee>=0),provider_pending BIGINT NOT NULL CHECK(provider_pending>=0),
 provider_owner_id TEXT NOT NULL REFERENCES owners(owner_id),settled_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 CHECK(platform_fee::numeric+provider_pending::numeric=charge::numeric)
);
CREATE TRIGGER job_settlement_immutable BEFORE UPDATE OR DELETE ON job_settlements FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
