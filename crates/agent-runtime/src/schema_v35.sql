ALTER TABLE runtime_model_observations ADD COLUMN adapter_code TEXT
CHECK(adapter_code IS NULL OR (kind='gateway_failed' AND adapter_code IN ('unclassified','identity_changed','invalid','unavailable','protocol','limit','incomplete','cancelled')));
PRAGMA user_version=35;
