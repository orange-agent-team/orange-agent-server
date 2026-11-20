CREATE TABLE connector_releases (
    provider text NOT NULL,release_id text NOT NULL,api_revision text NOT NULL,
    allowed_actions jsonb NOT NULL CHECK(jsonb_typeof(allowed_actions)='object' AND allowed_actions<>'{}'::jsonb),
    PRIMARY KEY(provider,release_id)
);
-- The whole map is immutable. Appending an action requires a different release row.
CREATE VIEW connector_release_actions AS
SELECT r.provider,r.release_id,a.key AS action_name,a.value AS execution_digest
FROM connector_releases r CROSS JOIN LATERAL jsonb_each_text(r.allowed_actions) a;
-- BEGIN RELEASE SEED
INSERT INTO connector_releases(provider,release_id,api_revision,allowed_actions) VALUES('github','1.0.0','GitHub.com REST 2022-11-28','{"list_issues":"c37cfb125ca4a984fda6e56900cc9fb2e097c487b9e2be8ea1efdbc2c8133967","list_pull_requests":"bf6dbd9979123af72fa486d4c092d811f3693ab90b1fafd66d62f6986b1394d7","list_repositories":"8bc5c6649f3bbeeb766efc9552f09cf6187db3747acb873e10da66e66b2ce25f","read_file":"7ed205c083daab6863f1ff793fdb336182c7a9f23b362156eb3e0475b9f1da20","read_issue":"2c5921a27688c327a771e3f517bbb961c228017b2e9aa2227b64c73cfc633482","read_pull_request":"ad59d134249d3e4a28d7c6f3c2d62dd317641784f9315a028e0c659f5999dd2b"}'::jsonb);
INSERT INTO connector_releases(provider,release_id,api_revision,allowed_actions) VALUES('gitlab','1.0.0','GitLab.com REST v4','{"list_issues":"ccfbb832c14ab953a6a1d9a781132c0b3df180801d0345a528fdc89f974b47f3","list_merge_requests":"4ae3345e092baea8f2ce5b1fd1c99d5dbefb106cc75686a75af73a104542d702","list_projects":"249986e89076f92911c79e54598663127c807b6d66433c5990ec0e077c3ddd4b","read_file":"705f06beae9c4cec73d20ff9ffee7be920710490a29b48b6cff5921d8f250860","read_issue":"fd2e5da32570ce321b00616169947d70bab0def1fcbceef225cdfdf76cafcf76","read_merge_request":"66b695adbe7d92610c8c14ce1b654363a66ca8b64bcd41c422f9ae42a5f6f6e6"}'::jsonb);
INSERT INTO connector_releases(provider,release_id,api_revision,allowed_actions) VALUES('notion','1.0.0','Notion 2026-03-11','{"list_blocks":"e7ad30626e4a040ac7f67c0302f83d6d5f730dee15d114b55e38c81d75396eb4","query_database":"f3d375dcb7436b17db3764524a0267b314c07273cd8675468db748427467dc31","read_page":"eabb3d8265b59431c85006c1ff396383169f4b360360ef14e202dd01528bb92f","search_pages":"0c56a9100cbd9f28b9d12e8c1834fe04fd15e109de3ea3b26b19731763d63148"}'::jsonb);
INSERT INTO connector_releases(provider,release_id,api_revision,allowed_actions) VALUES('confluence','1.0.0','Confluence Cloud REST v2 pages, spaces, hierarchy and attachments; v1 CQL and attachment download','{"list_children":"95455cb699c83866966591861a4b9b988f395e80aae2d9375a4e9283aa196419","read_attachment":"4c7075bbac24993b9f2f470d495c3f5b57036d42e17794eed3bfdc49eadae9d8","read_page":"b5908fcd3683b4e3937a430c5229c39edf2cbec39fb68bbecd32f0f369b84d23","search_pages":"b0561be5985b67b5d034ad3fea4d2f0508fde9ee619a3d9531ebb534d49f9ed8"}'::jsonb);
INSERT INTO connector_releases(provider,release_id,api_revision,allowed_actions) VALUES('postgres','1.0.0','PostgreSQL wire protocol; PostgreSQL 17 development profile','{"describe_view":"41f612e20942059e7da0f4a0ff492759c7e51271b1a2247d0838301936acb1e0","list_views":"aaec83fa25e27f8b8b69080deb5126bc876cbea137ca7becd9ec117e901098b6","query_view":"bdd70a5a998178b361d7f43ddd18b54ccd7aaf15f9fa7b58233c2262d01e84e0"}'::jsonb);
INSERT INTO connector_releases(provider,release_id,api_revision,allowed_actions) VALUES('s3','1.0.0','S3 Signature V4, ListObjectsV2, conditional GET/HEAD','{"head_object":"5867eccaa775b19452c65499934b3fc14e4fc513e09c6b259e29394b87139f22","list_objects":"1671920cab5e5452e30a7314f8dd44f10cecebdb728c5087ae57040b8e767d0a","read_object":"bb8e41354b3950427329db0e900280e12e8876859ae60fa091b52fefec24a39e"}'::jsonb);
-- END RELEASE SEED
CREATE TABLE connector_grant_release (
    provider text NOT NULL, grant_id text NOT NULL,release_id text NOT NULL,
    PRIMARY KEY(provider,grant_id), FOREIGN KEY(provider,release_id) REFERENCES connector_releases
);
INSERT INTO connector_grant_release SELECT 'postgres',grant_id,'1.0.0' FROM pg_view_grants;
INSERT INTO connector_grant_release SELECT 's3',grant_id,'1.0.0' FROM s3_source_grants;
INSERT INTO connector_grant_release SELECT provider,grant_id,'1.0.0' FROM repository_source_grants;
-- Legacy INSERT statements keep the original read-only grant semantics on new binaries.
CREATE FUNCTION bind_first_connector_release() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE family text;
BEGIN
    IF TG_NARGS=1 THEN family:=TG_ARGV[0]; ELSE family:=NEW.provider; END IF;
    INSERT INTO connector_grant_release(provider,grant_id,release_id) VALUES(family,NEW.grant_id,'1.0.0');
    RETURN NEW;
END $$;
CREATE TRIGGER pg_grant_release AFTER INSERT ON pg_view_grants FOR EACH ROW EXECUTE FUNCTION bind_first_connector_release('postgres');
CREATE TRIGGER s3_grant_release AFTER INSERT ON s3_source_grants FOR EACH ROW EXECUTE FUNCTION bind_first_connector_release('s3');
CREATE TRIGGER external_grant_release AFTER INSERT ON repository_source_grants FOR EACH ROW EXECUTE FUNCTION bind_first_connector_release();
CREATE TRIGGER connector_release_immutable BEFORE UPDATE OR DELETE ON connector_releases FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER connector_binding_immutable BEFORE UPDATE OR DELETE ON connector_grant_release FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION connector_grant_allows(family text, grant_ref text, action text, required_digest text)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS(SELECT FROM connector_grant_release b JOIN connector_release_actions a USING(provider,release_id)
        WHERE b.provider=family AND b.grant_id=grant_ref AND a.action_name=action AND a.execution_digest=required_digest)
$$;
