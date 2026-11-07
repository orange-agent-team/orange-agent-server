CREATE FUNCTION protect_artifact_transfer() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF ROW(NEW.transfer_id,NEW.artifact_id,NEW.stage_key,NEW.published_key) IS DISTINCT FROM ROW(OLD.transfer_id,OLD.artifact_id,OLD.stage_key,OLD.published_key)
 OR NEW.reserved_bytes>OLD.reserved_bytes OR (OLD.stage_deleted AND NOT NEW.stage_deleted)
 OR (OLD.published_deleted AND NOT NEW.published_deleted) OR (OLD.state='purged' AND NEW.state!='purged')
 THEN RAISE EXCEPTION 'artifact transfer identity/reclamation is irreversible'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER artifact_transfer_identity BEFORE UPDATE ON artifact_transfers FOR EACH ROW EXECUTE FUNCTION protect_artifact_transfer();
CREATE FUNCTION protect_artifact_pin() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 PERFORM artifact_id FROM artifacts WHERE artifact_id=NEW.artifact_id AND state='available' FOR SHARE;
 IF NOT FOUND THEN RAISE EXCEPTION 'only available artifacts may be pinned'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER artifact_pin_live BEFORE INSERT OR UPDATE ON artifact_pins FOR EACH ROW EXECUTE FUNCTION protect_artifact_pin();
