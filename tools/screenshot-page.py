"""Render the configuration page to docs/config-page.png.

The page's API calls carry an `X-Dsh-Launcher` header that a plain browser
screenshot cannot send, so a tiny reverse proxy on 127.0.0.1:3091 adds it and
headless Edge shoots the proxied copy. Run with the tray (and its config page)
already running.
"""

import http.server
import os
import subprocess
import threading
import urllib.request

UPSTREAM = "http://127.0.0.1:3089"
EDGE = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
OUT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "docs", "config-page.png")


class Proxy(http.server.BaseHTTPRequestHandler):
    def _relay(self, method: str) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        request = urllib.request.Request(UPSTREAM + self.path, data=body, method=method)
        for name, value in self.headers.items():
            if name.lower() in ("host", "x-dsh-launcher", "content-length", "connection"):
                continue
            request.add_header(name, value)
        request.add_header("X-Dsh-Launcher", "1")
        request.add_header("Host", "127.0.0.1:3089")
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                payload = response.read()
                code = response.status
                kind = response.headers.get("Content-Type", "text/plain")
        except urllib.error.HTTPError as error:
            payload = error.read()
            code = error.code
            kind = error.headers.get("Content-Type", "text/plain")
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802 - http.server API
        self._relay("GET")

    def do_POST(self) -> None:  # noqa: N802 - http.server API
        self._relay("POST")

    def log_message(self, *_args) -> None:
        pass


def main() -> None:
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 3091), Proxy)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    if os.path.exists(OUT):
        os.remove(OUT)
    subprocess.run(
        [
            EDGE,
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--no-first-run",
            "--allow-insecure-localhost",
            "--window-size=1180,1560",
            f"--screenshot={OUT}",
            "http://127.0.0.1:3091/",
        ],
        check=True,
        timeout=90,
    )
    server.shutdown()
    print("wrote", OUT, os.path.getsize(OUT), "bytes")


if __name__ == "__main__":
    main()
