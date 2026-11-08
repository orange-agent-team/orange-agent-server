-- Mark only invocations/approvals that predate catalogue capture. A missing
-- snapshot on a newly-created invocation can never be treated as legacy.
CREATE TABLE runtime_catalog_legacy(
 invocation TEXT PRIMARY KEY REFERENCES runtime_invocations(invocation)
);
INSERT INTO runtime_catalog_legacy SELECT invocation FROM runtime_invocations;
CREATE TABLE runtime_catalog_legacy_approvals(
 operation_id TEXT PRIMARY KEY REFERENCES operations(operation_id),
 invocation TEXT NOT NULL, tool_version TEXT NOT NULL,
 actor TEXT NOT NULL, argument_digest TEXT NOT NULL, environment TEXT NOT NULL,
 policy TEXT NOT NULL, expires_at INTEGER NOT NULL
);
INSERT INTO runtime_catalog_legacy_approvals
 SELECT o.operation_id,o.invocation,o.tool_version,a.actor,a.argument_digest,a.environment,a.policy,a.expires_at
 FROM operations o JOIN approvals a USING(operation_id) JOIN runtime_invocations i ON i.invocation=o.invocation
 WHERE o.request_json IS NOT NULL;
CREATE TABLE runtime_tool_catalogs(
 invocation TEXT PRIMARY KEY REFERENCES runtime_invocations(invocation),
 revision TEXT NOT NULL CHECK(length(revision)=64),
 document_digest TEXT NOT NULL CHECK(length(document_digest)=64),
 document TEXT NOT NULL CHECK(json_valid(document)),
 origin TEXT NOT NULL CHECK(origin IN('dispatch','legacy_builtin_v1'))
);
CREATE TABLE runtime_tool_approvals(
 operation_id TEXT PRIMARY KEY REFERENCES operations(operation_id),
 invocation TEXT NOT NULL REFERENCES runtime_tool_catalogs(invocation),
 tool_version TEXT NOT NULL, execution_digest TEXT NOT NULL CHECK(length(execution_digest)=64)
);
CREATE TRIGGER runtime_catalog_legacy_no_update BEFORE UPDATE ON runtime_catalog_legacy BEGIN SELECT RAISE(ABORT,'immutable legacy eligibility');END;
CREATE TRIGGER runtime_catalog_legacy_no_delete BEFORE DELETE ON runtime_catalog_legacy BEGIN SELECT RAISE(ABORT,'legacy eligibility retained');END;
CREATE TRIGGER runtime_catalog_legacy_approval_no_update BEFORE UPDATE ON runtime_catalog_legacy_approvals BEGIN SELECT RAISE(ABORT,'immutable legacy approval');END;
CREATE TRIGGER runtime_catalog_legacy_approval_no_delete BEFORE DELETE ON runtime_catalog_legacy_approvals BEGIN SELECT RAISE(ABORT,'legacy approval retained');END;
CREATE TRIGGER runtime_catalog_no_update BEFORE UPDATE ON runtime_tool_catalogs BEGIN SELECT RAISE(ABORT,'immutable invocation catalogue');END;
CREATE TRIGGER runtime_catalog_no_delete BEFORE DELETE ON runtime_tool_catalogs BEGIN SELECT RAISE(ABORT,'invocation catalogue retained');END;
CREATE TRIGGER runtime_tool_approval_no_update BEFORE UPDATE ON runtime_tool_approvals BEGIN SELECT RAISE(ABORT,'immutable tool approval binding');END;
CREATE TRIGGER runtime_tool_approval_no_delete BEFORE DELETE ON runtime_tool_approvals BEGIN SELECT RAISE(ABORT,'tool approval binding retained');END;
PRAGMA user_version=7;
