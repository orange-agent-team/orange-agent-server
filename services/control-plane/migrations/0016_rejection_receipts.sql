ALTER TABLE worker_sessions ADD COLUMN rejection_receipts_version INTEGER NOT NULL DEFAULT 0 CHECK(rejection_receipts_version BETWEEN 0 AND 1);
ALTER TABLE worker_execution_attempts ADD COLUMN rejection_result INTEGER CHECK(rejection_result IN (2,3));
ALTER TABLE worker_execution_attempts ADD COLUMN rejection_receipt_id TEXT UNIQUE;
-- Old rejected attempts may not yet have a receipt. A current authenticated
-- historical query may materialize it only after matching the ORIGINAL ACK hash.
ALTER TABLE worker_execution_attempts ADD CONSTRAINT rejection_receipt_pair CHECK((rejection_result IS NULL)=(rejection_receipt_id IS NULL));
ALTER TABLE worker_execution_attempts ADD CONSTRAINT rejection_receipt_state CHECK(rejection_receipt_id IS NULL OR state='rejected');
CREATE FUNCTION protect_rejection_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.rejection_receipt_id IS NOT NULL AND ROW(NEW.rejection_receipt_id,NEW.rejection_result) IS DISTINCT FROM ROW(OLD.rejection_receipt_id,OLD.rejection_result)
 THEN RAISE EXCEPTION 'rejection receipt is immutable'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER rejection_receipt_immutable BEFORE UPDATE ON worker_execution_attempts
FOR EACH ROW EXECUTE FUNCTION protect_rejection_receipt();
