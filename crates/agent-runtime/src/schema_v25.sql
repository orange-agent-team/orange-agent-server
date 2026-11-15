-- A semantic format boundary: old executors must not reopen a mixed Run and
-- interpret it without actual dependency lifetimes and worker drain rules.
CREATE TABLE runtime_effect_policy(singleton INTEGER PRIMARY KEY CHECK(singleton=1),policy TEXT NOT NULL CHECK(policy='source-lifetime.v1'));
INSERT INTO runtime_effect_policy VALUES(1,'source-lifetime.v1');
CREATE TRIGGER effect_policy_no_update BEFORE UPDATE ON runtime_effect_policy BEGIN SELECT RAISE(ABORT,'immutable effect lifetime policy');END;
CREATE TRIGGER effect_policy_no_delete BEFORE DELETE ON runtime_effect_policy BEGIN SELECT RAISE(ABORT,'effect lifetime policy retained');END;
PRAGMA user_version=25;
