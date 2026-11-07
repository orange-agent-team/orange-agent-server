-- Bound a current submitter's metadata cursor scan to their own owner/user key.
CREATE INDEX user_job_listing ON jobs(owner_id,submitting_user_id,job_id)
WHERE submitting_user_id IS NOT NULL;
