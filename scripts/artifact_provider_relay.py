"""Hold an actual private provider HEAD reply while another client revokes the requesting session."""
from contextlib import contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Event, Thread
from urllib.parse import urlsplit


class Fence:
    def __init__(self):
        self.path=None
        self.received=Event()
        self.release=Event()


@contextmanager
def relay(endpoint,bucket):
    addr=urlsplit(endpoint)
    if addr.hostname!="127.0.0.1" or addr.scheme!="http":raise ValueError("Local fixture required")
    fence=Fence()
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args): pass

        def forward(self):
            route=urlsplit(self.path)
            if route.scheme or route.netloc or (route.path!="/"+bucket and not route.path.startswith("/"+bucket+"/")):
                self.send_error(403);return
            length=int(self.headers.get("Content-Length",0))
            if length<0 or length>(16<<20) or self.headers.get("Transfer-Encoding"):
                self.send_error(413);return
            upstream=HTTPConnection("127.0.0.1",addr.port,timeout=20)
            try:
                data=self.rfile.read(length) if length else None
                upstream.request(self.command,self.path,body=data,headers=dict(self.headers.items()))
                response=upstream.getresponse()
                content=response.read((16<<20)+1)
                if len(content)>(16<<20):self.send_error(413);return
                if self.command=="HEAD" and route.path==fence.path and response.status==200 and "versionId=" in route.query:
                    fence.received.set()
                    if not fence.release.wait(18):self.send_error(504);return
                self.send_response(response.status)
                for name,value in response.getheaders():
                    if name.lower() not in ("server","date","connection","transfer-encoding","content-length"):
                        self.send_header(name,value)
                self.send_header("Content-Length",response.getheader("Content-Length","0") if self.command=="HEAD" else str(len(content)))
                self.end_headers()
                if self.command!="HEAD":self.wfile.write(content)
            finally:upstream.close()
        do_GET=do_HEAD=do_PUT=do_POST=do_DELETE=do_OPTIONS=forward
    with ThreadingHTTPServer(("127.0.0.1",0),Handler) as server:
        thread=Thread(target=server.serve_forever,daemon=True);thread.start()
        try:yield f"http://127.0.0.1:{server.server_port}",fence
        finally:fence.release.set();server.shutdown();thread.join(timeout=5)
