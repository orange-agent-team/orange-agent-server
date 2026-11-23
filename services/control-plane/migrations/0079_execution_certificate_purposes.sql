-- A trusted TLS leaf has one enrollment purpose and one subject throughout its lifetime.
-- Acquire both registration tables before inspecting pre-existing rows or enabling the invariant.
LOCK TABLE worker_credentials,cloud_pool_hosts IN SHARE ROW EXCLUSIVE MODE;

CREATE TABLE execution_certificate_purposes (
    certificate_sha256 text PRIMARY KEY CHECK(certificate_sha256 ~ '^[0-9a-f]{64}$'),
    purpose text NOT NULL CHECK(purpose IN ('public_worker','platform_host','retired_conflict')),
    subject_id text,
    enrollment_user_id text,
    CHECK((purpose='public_worker' AND subject_id IS NOT NULL AND enrollment_user_id IS NOT NULL)
       OR (purpose='platform_host' AND subject_id IS NOT NULL AND enrollment_user_id IS NULL)
       OR (purpose='retired_conflict' AND subject_id IS NULL AND enrollment_user_id IS NULL))
);

WITH prior AS (
    SELECT certificate_sha256,'public_worker' AS purpose,node_id AS subject_id,enrollment_user_id FROM worker_credentials
    UNION ALL
    SELECT certificate_sha256,'platform_host',host_instance_id,NULL FROM cloud_pool_hosts
)
INSERT INTO execution_certificate_purposes
SELECT certificate_sha256,CASE WHEN count(*)=1 THEN min(purpose) ELSE 'retired_conflict' END,
    CASE WHEN count(*)=1 THEN min(subject_id) END, CASE WHEN count(*)=1 THEN min(enrollment_user_id) END
FROM prior GROUP BY certificate_sha256;

UPDATE worker_credentials SET active=false WHERE certificate_sha256 IN
    (SELECT certificate_sha256 FROM execution_certificate_purposes WHERE purpose='retired_conflict');
UPDATE worker_sessions SET expires_at=least(expires_at,clock_timestamp()) WHERE certificate_sha256 IN
    (SELECT certificate_sha256 FROM execution_certificate_purposes WHERE purpose='retired_conflict');
UPDATE cloud_pool_hosts SET enabled=false,accepting=false,live_until=NULL WHERE certificate_sha256 IN
    (SELECT certificate_sha256 FROM execution_certificate_purposes WHERE purpose='retired_conflict');

CREATE FUNCTION preserve_execution_certificate_purpose() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'execution certificate purpose is immutable' USING ERRCODE='23514';
END $$;
CREATE TRIGGER immutable_certificate_purpose BEFORE UPDATE OR DELETE ON execution_certificate_purposes
    FOR EACH ROW EXECUTE FUNCTION preserve_execution_certificate_purpose();
CREATE TRIGGER immutable_certificate_purpose_table BEFORE TRUNCATE ON execution_certificate_purposes
    FOR EACH STATEMENT EXECUTE FUNCTION preserve_execution_certificate_purpose();

CREATE FUNCTION bind_execution_certificate_purpose() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    kind text := TG_ARGV[0];
    subject text;
    enrolled text;
BEGIN
    IF kind='public_worker' THEN
        subject := NEW.node_id;
        enrolled := NEW.enrollment_user_id;
    ELSE
        subject := NEW.host_instance_id;
    END IF;
    INSERT INTO execution_certificate_purposes(certificate_sha256,purpose,subject_id,enrollment_user_id)
        VALUES(NEW.certificate_sha256,kind,subject,enrolled) ON CONFLICT(certificate_sha256) DO NOTHING;
    IF NOT EXISTS (SELECT FROM execution_certificate_purposes p WHERE p.certificate_sha256=NEW.certificate_sha256
            AND p.purpose=kind AND p.subject_id=subject AND p.enrollment_user_id IS NOT DISTINCT FROM enrolled) THEN
        RAISE EXCEPTION 'certificate already bound to a different purpose or subject' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER bind_public_worker_leaf BEFORE INSERT OR UPDATE OF certificate_sha256,node_id,enrollment_user_id,active
    ON worker_credentials FOR EACH ROW EXECUTE FUNCTION bind_execution_certificate_purpose('public_worker');
CREATE TRIGGER bind_platform_host_leaf BEFORE INSERT OR UPDATE OF certificate_sha256,host_instance_id,enabled
    ON cloud_pool_hosts FOR EACH ROW EXECUTE FUNCTION bind_execution_certificate_purpose('platform_host');
ALTER TABLE worker_credentials ADD FOREIGN KEY(certificate_sha256) REFERENCES execution_certificate_purposes(certificate_sha256);
ALTER TABLE cloud_pool_hosts ADD FOREIGN KEY(certificate_sha256) REFERENCES execution_certificate_purposes(certificate_sha256);
