-- A response commit must reference the exact artifact version bound to its Job,
-- even if future application code accidentally mixes otherwise valid IDs.
ALTER TABLE artifacts ADD CONSTRAINT artifact_exact_version UNIQUE(artifact_id,version_id);
ALTER TABLE worker_output_uploads ADD CONSTRAINT output_exact_job_artifact UNIQUE(job_id,artifact_id);
ALTER TABLE job_response_commits ADD CONSTRAINT response_exact_artifact_version
 FOREIGN KEY(artifact_id,version_id) REFERENCES artifacts(artifact_id,version_id);
ALTER TABLE job_response_commits ADD CONSTRAINT response_exact_job_output
 FOREIGN KEY(job_id,artifact_id) REFERENCES worker_output_uploads(job_id,artifact_id);
