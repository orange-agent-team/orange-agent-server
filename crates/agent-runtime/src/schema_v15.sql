-- Persistent payload revision: new patch approvals bind the native leaf name,
-- publication plans pin it before dispatch. Old payloads are not backfilled.
-- Old binaries must refuse this journal rather than guess missing semantics.
PRAGMA user_version=15;
