"""Owned loopback relay that can delay delivery after a real signed object read."""

import http.client
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import time


@contextmanager
def delayed_source(port, folder):
    class Relay(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def forward(self):
            upstream = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
            try:
                upstream.request(
                    self.command, self.path, headers=dict(self.headers.items())
                )
                response = upstream.getresponse()
                data = response.read(20 * 1024 * 1024 + 1)
                if len(data) > 20 * 1024 * 1024:
                    self.send_error(413)
                    return
                if (
                    self.command == "GET"
                    and response.status == 200
                    and self.path.endswith("/medium.bin")
                    and (folder / "arm-revoke").exists()
                ):
                    if len(data) != 3 * 1024 * 1024:
                        raise ValueError("Incomplete upstream content")
                    (folder / "upstream-read").touch()
                    deadline = time.monotonic() + 10
                    while (
                        not (folder / "release-get").exists()
                        and time.monotonic() < deadline
                    ):
                        time.sleep(0.01)
                self.send_response(response.status)
                for key, value in response.getheaders():
                    if key.lower() not in {
                        "connection",
                        "transfer-encoding",
                        "date",
                        "server",
                        "content-length",
                    }:
                        self.send_header(key, value)
                self.send_header(
                    "Content-Length",
                    response.getheader("Content-Length", "0")
                    if self.command == "HEAD"
                    else str(len(data)),
                )
                self.end_headers()
                if data:
                    self.wfile.write(data)
            finally:
                upstream.close()

        do_GET = forward
        do_HEAD = forward

    with ThreadingHTTPServer(("127.0.0.1", 0), Relay) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield server.server_port
        finally:
            (folder / "release-get").touch()
            server.shutdown()
            thread.join(timeout=5)
