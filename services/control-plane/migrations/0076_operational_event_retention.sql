-- Age-driven retention applies only to named operational notification streams.
-- The ledger, security audit, object tombstones and consumers' inboxes have separate owners.
CREATE INDEX operational_notification_age ON durable_events(occurred_at,stream_id,seq);

-- Keep a bounded per-stream fault marker, so an unresolved damaged prefix cannot
-- monopolize the next selection. A healthy contiguous prefix clears its marker.
CREATE TABLE operational_retention_faults (
    stream_id text PRIMARY KEY REFERENCES event_streams(stream_id),
    last_observed timestamptz NOT NULL
);

CREATE FUNCTION orange_prune_old_notifications(p_stream_limit integer,p_event_limit integer)
RETURNS TABLE(stream_id text,removed integer,integrity_blocked boolean)
LANGUAGE plpgsql AS $$
DECLARE
    cutoff timestamptz := statement_timestamp()-interval '30 days';
    candidate record;
    ceiling bigint;
    through_seq bigint;
    paired integer;
    amount integer;
BEGIN
    IF p_stream_limit IS NULL OR p_event_limit IS NULL OR p_stream_limit NOT BETWEEN 1 AND 32
        OR p_event_limit NOT BETWEEN 1 AND 256 THEN
        RAISE EXCEPTION 'invalid operational retention bounds' USING ERRCODE='22023';
    END IF;
    FOR candidate IN
        SELECT s.stream_id,s.oldest_seq,s.next_seq FROM event_streams s
        LEFT JOIN durable_events head ON head.stream_id=s.stream_id AND head.seq=s.oldest_seq
        LEFT JOIN outbox receipt ON receipt.event_id=head.source_outbox_id AND receipt.domain_event_id=head.event_id
        LEFT JOIN operational_retention_faults fault ON fault.stream_id=s.stream_id
        WHERE s.entity_kind IN ('job','node','worker') AND s.oldest_seq<s.next_seq AND
              (head.event_id IS NULL OR (head.occurred_at<=cutoff AND (receipt.event_id IS NULL OR receipt.delivered_at<=cutoff)))
        ORDER BY fault.last_observed NULLS FIRST,head.occurred_at,s.stream_id LIMIT p_stream_limit FOR UPDATE OF s SKIP LOCKED
    LOOP
        ceiling:=candidate.oldest_seq+least(candidate.next_seq-1-candidate.oldest_seq,p_event_limit::bigint-1);
        -- A recent, undelivered or recently delivered row stops the prefix even if later
        -- sequences have older timestamps. Lock receipt rows until the cursor commits.
        WITH locked AS (
            SELECT e.seq,e.occurred_at,o.delivered_at FROM durable_events e JOIN outbox o
            ON o.event_id=e.source_outbox_id AND o.domain_event_id=e.event_id
            WHERE e.stream_id=candidate.stream_id AND e.seq BETWEEN candidate.oldest_seq AND ceiling
            ORDER BY e.seq FOR SHARE OF o
        )
        SELECT count(*)::integer,coalesce(min(l.seq) FILTER(WHERE l.occurred_at>cutoff OR
            l.delivered_at IS NULL OR l.delivered_at>cutoff)-1,ceiling)
            INTO paired,through_seq FROM locked l;
        stream_id:=candidate.stream_id;
        IF paired<>ceiling-candidate.oldest_seq+1 THEN
            INSERT INTO operational_retention_faults VALUES(candidate.stream_id,clock_timestamp())
                ON CONFLICT ON CONSTRAINT operational_retention_faults_pkey DO UPDATE SET last_observed=EXCLUDED.last_observed;
            removed:=0;integrity_blocked:=true;RETURN NEXT;CONTINUE;
        END IF;
        SELECT b.removed INTO amount FROM orange_prune_event_batch(candidate.stream_id,through_seq,p_event_limit) b;
        DELETE FROM operational_retention_faults f WHERE f.stream_id=candidate.stream_id;
        removed:=amount;integrity_blocked:=false;RETURN NEXT;
    END LOOP;
END $$;
REVOKE ALL ON FUNCTION orange_prune_old_notifications(integer,integer) FROM PUBLIC;
