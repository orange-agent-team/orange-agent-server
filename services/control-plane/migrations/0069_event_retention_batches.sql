-- Keyset access for account statement readers and expired staging candidates.
CREATE INDEX ledger_owner_posted_idx ON ledger_transactions(owner_id, created_at DESC, transaction_id DESC);
CREATE INDEX artifact_staging_reclaim_idx ON artifacts(root_id, created_at, artifact_id) WHERE state='staging';

-- One operator-selected stream and at most 256 contiguous notifications per call.
-- This function uses the caller's table privileges and has no public execution grant.
CREATE FUNCTION orange_prune_event_batch(p_stream text, p_through bigint, p_limit integer)
RETURNS TABLE(removed integer, oldest_remaining bigint, blocked_at bigint, complete boolean)
LANGUAGE plpgsql AS $$
DECLARE
    head bigint;
    next_seq_value bigint;
    stop_seq bigint;
    missing bigint;
    pending bigint;
    count_deleted bigint;
BEGIN
    IF p_stream IS NULL OR p_through IS NULL OR p_limit IS NULL OR p_through < 0 OR p_limit NOT BETWEEN 1 AND 256 THEN
        RAISE EXCEPTION 'invalid retention bounds' USING ERRCODE='22023';
    END IF;
    SELECT s.oldest_seq,s.next_seq INTO STRICT head,next_seq_value FROM event_streams s WHERE s.stream_id=p_stream FOR UPDATE;
    IF p_through >= next_seq_value THEN
        RAISE EXCEPTION 'retention cursor exceeds committed stream' USING ERRCODE='22023';
    END IF;
    IF head > p_through THEN
        RETURN QUERY SELECT 0,head,NULL::bigint,true;
        RETURN;
    END IF;
    stop_seq := head + LEAST(p_through-head,p_limit::bigint-1);
    SELECT min(n.seq) FILTER (WHERE e.event_id IS NULL OR o.event_id IS NULL),
           min(n.seq) FILTER (WHERE o.delivered_at IS NULL)
      INTO missing,pending
      FROM generate_series(head,stop_seq) AS n(seq)
      LEFT JOIN durable_events e ON e.stream_id=p_stream AND e.seq=n.seq
      LEFT JOIN outbox o ON o.event_id=e.source_outbox_id AND o.domain_event_id=e.event_id;
    IF missing IS NOT NULL THEN
        RAISE EXCEPTION 'retention prefix is missing an event or original outbox record' USING ERRCODE='XX001';
    END IF;
    IF pending IS NOT NULL THEN stop_seq := pending-1; END IF;
    IF stop_seq >= head THEN
        DELETE FROM outbox o USING durable_events e
         WHERE e.stream_id=p_stream AND e.seq BETWEEN head AND stop_seq
           AND o.event_id=e.source_outbox_id AND o.domain_event_id=e.event_id AND o.delivered_at IS NOT NULL;
        GET DIAGNOSTICS count_deleted=ROW_COUNT;
        IF count_deleted <> stop_seq-head+1 THEN
            RAISE EXCEPTION 'outbox prefix changed' USING ERRCODE='40001';
        END IF;
        DELETE FROM durable_events e WHERE e.stream_id=p_stream AND e.seq BETWEEN head AND stop_seq;
        GET DIAGNOSTICS count_deleted=ROW_COUNT;
        IF count_deleted <> stop_seq-head+1 THEN
            RAISE EXCEPTION 'event prefix changed' USING ERRCODE='40001';
        END IF;
        UPDATE event_streams s SET oldest_seq=stop_seq+1 WHERE s.stream_id=p_stream;
    END IF;
    RETURN QUERY SELECT (stop_seq-head+1)::integer,stop_seq+1,pending,stop_seq>=p_through;
END $$;
REVOKE ALL ON FUNCTION orange_prune_event_batch(text,bigint,integer) FROM PUBLIC;
