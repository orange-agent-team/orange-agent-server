#!/usr/bin/env python3
"""Exercise the guest's sole control reader during preparation, ready state, expiry and stop."""
from contextlib import contextmanager
import os
import time
from cloud.qualify_guest import GuestLease


@contextmanager
def channel(seconds=3):
    r, w = os.pipe()
    with os.fdopen(r, "rb") as reader, os.fdopen(w, "wb", buffering=0) as writer:
        until = int(time.time()) + seconds
        lease = GuestLease(reader, until, until + 10)
        try:
            yield lease, writer, until
        finally:
            writer.close()
            lease.reader.join(timeout=2)
            if lease.reader.is_alive():
                raise RuntimeError("Control thread failed to observe EOF")


def observed(lease, predicate):
    with lease.changed:
        if not lease.changed.wait_for(predicate, timeout=2):
            raise RuntimeError("Control frame was not observed")


def refused(lease):
    try:
        lease.mark_ready()
    except RuntimeError:
        return
    raise AssertionError("Expired or stopped workspace obtained readiness")


with channel() as (lease, writer, original):
    writer.write(f"renew {original + 3}\nrenew {original + 6}\n".encode())
    observed(lease, lambda: lease.expires == original + 6)
    time.sleep(max(0, original - time.time()) + 0.05)
    lease.mark_ready()
    writer.write(b'tool {"operation_id":"fixture"}\n')
    if lease.next_tool() != {"operation_id":"fixture"} or not lease.authorizes(original+5):
        raise RuntimeError("Current tool or extended expiry was lost")
    writer.write(b"stop\n")
    observed(lease, lambda: lease.stopped)
    if lease.next_tool() is not None or lease.authorizes(original+5):
        raise RuntimeError("Unexpected command after stop")

with channel(2) as (lease, writer, original):
    time.sleep(max(0, original - time.time()) + 0.05)
    writer.write(f"renew {original + 4}\n".encode())
    observed(lease, lambda: lease.stopped)
    refused(lease)

for message in [b"stop\n", b"tool {}\n", b"renew -1\n", b"x"*2049, b"renew 99999999999\n"]:
    with channel() as (lease, writer, _):
        writer.write(message)
        observed(lease, lambda: lease.stopped)
        refused(lease)

print("PASS buffered renewal while preparing, expired original after timely renewal, no revival, "
      "no early tool, bounded commands, explicit stop and EOF cleanup")
