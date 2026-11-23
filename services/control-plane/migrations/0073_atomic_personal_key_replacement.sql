ALTER TABLE personal_automation_keys ADD COLUMN rotated_from uuid;
ALTER TABLE personal_automation_keys ADD CONSTRAINT distinct_personal_key_replacement
    CHECK (rotated_from IS NULL OR (verifier IS NOT NULL AND rotated_from<>key_id));
ALTER TABLE personal_automation_keys ADD CONSTRAINT owned_personal_key_predecessor
    FOREIGN KEY (user_id,rotated_from) REFERENCES personal_automation_keys(user_id,key_id)
    DEFERRABLE INITIALLY DEFERRED;
CREATE UNIQUE INDEX personal_key_one_replacement ON personal_automation_keys(user_id,rotated_from)
    WHERE rotated_from IS NOT NULL;

-- Only a newly inserted successor may retire the original. A replay handled by
-- ON CONFLICT DO NOTHING never enters this trigger.
CREATE FUNCTION retire_replaced_personal_key() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE personal_automation_keys SET revoked_unix=floor(extract(epoch FROM clock_timestamp()))
     WHERE user_id=NEW.user_id AND owner_id=NEW.owner_id AND key_id=NEW.rotated_from
       AND revoked_unix IS NULL AND scope=NEW.scope AND expires_unix>floor(extract(epoch FROM clock_timestamp()));
    IF NOT FOUND THEN
        RAISE EXCEPTION 'predecessor is not eligible for replacement' USING ERRCODE='OA073';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER retire_personal_key_after_replacement AFTER INSERT ON personal_automation_keys
FOR EACH ROW WHEN (NEW.rotated_from IS NOT NULL) EXECUTE FUNCTION retire_replaced_personal_key();
