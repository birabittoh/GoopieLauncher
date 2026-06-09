"""A tiny threaded HTTP server that imitates the GitHub releases API the
launcher queries, plus the downloadable asset. Used by the self-update e2e test
so nothing touches the network.

Routes:
  GET /releases.json      -> a release whose tag is newer than any real version
  GET /assets/<name>      -> the dummy payload bytes the launcher will install
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class MockReleaseServer:
    def __init__(self, asset_name: str, asset_bytes: bytes, tag: str = "v9999.0.0") -> None:
        self.asset_name = asset_name
        self.asset_bytes = asset_bytes
        self.tag = tag
        self._httpd: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    @property
    def base_url(self) -> str:
        assert self._httpd is not None
        host, port = self._httpd.server_address
        return f"http://127.0.0.1:{port}"

    @property
    def releases_url(self) -> str:
        return f"{self.base_url}/releases.json"

    def _releases_json(self) -> bytes:
        body = {
            "tag_name": self.tag,
            "assets": [
                {
                    "name": self.asset_name,
                    "browser_download_url": f"{self.base_url}/assets/{self.asset_name}",
                }
            ],
        }
        return json.dumps(body).encode()

    def start(self) -> "MockReleaseServer":
        server = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):  # silence per-request logging
                pass

            def do_GET(self):  # noqa: N802 (http.server API)
                if self.path == "/releases.json":
                    payload = server._releases_json()
                    ctype = "application/json"
                elif self.path == f"/assets/{server.asset_name}":
                    payload = server.asset_bytes
                    ctype = "application/octet-stream"
                else:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)
        self._thread.start()
        return self

    def stop(self) -> None:
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()
            self._httpd = None
        if self._thread is not None:
            self._thread.join(timeout=5)
            self._thread = None

    def __enter__(self) -> "MockReleaseServer":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()
