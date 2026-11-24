"""Expose fixed denoising counters only, without consuming the final result stream."""
import json
import os

LIMIT = 65536


class StepObserver:
    def __init__(self, attempt):
        self.attempt = attempt
        self.offset = 0
        self.partial = b""
        self.completed = 0

    def read(self, descriptor):
        size = os.fstat(descriptor).st_size
        if not self.offset <= size <= LIMIT:
            raise ValueError("Invalid SDXL event stream size")
        data = os.pread(descriptor, size - self.offset, self.offset)
        self.offset += len(data)
        lines = (self.partial + data).split(b"\n")
        self.partial = lines.pop()
        new = []
        for line in lines:
            def unique(pairs):
                result = {}
                for key, value in pairs:
                    if key in result:
                        raise ValueError("Duplicate SDXL event field")
                    result[key] = value
                return result
            frame = json.loads(line, object_pairs_hook=unique)
            if not isinstance(frame, dict):
                raise ValueError("Unexpected event")
            if frame.get("phase") == "complete":
                if self.completed != 25:
                    raise ValueError("Early result")
                continue
            next_step = self.completed + 1
            if (next_step > 25 or type(frame.get("completed_steps")) is not int or
                frame != {"phase":"denoising", "attempt_id":self.attempt, "completed_steps":next_step}):
                raise ValueError("Unverified step")
            self.completed = next_step
            new.append(frame)
        return new
