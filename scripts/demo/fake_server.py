from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
from urllib.parse import urlparse

DATA_DIR = Path(__file__).resolve().parents[2] / "testdata" / "demo" / "api"
ROUTES = {
    "/api/v1/auth/me": "me.json",
    "/api/v1/subscriptions/summary": "subscriptions.json",
    "/api/v1/keys": "keys.json",
    "/api/v1/usage/dashboard/trend": "usage.json",
    "/api/v1/groups/available": "groups.json",
}

def load(name):
    return json.loads((DATA_DIR / name).read_text(encoding="utf-8"))

class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def send_json(self, value, status=200, envelope=True):
        if envelope:
            value = {"code": 0, "message": "success", "data": value}
        body = json.dumps(value, ensure_ascii=True).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/user/balance":
            self.send_json(load("deepseek_balance.json"), envelope=False)
            return
        route = ROUTES.get(path)
        if route:
            self.send_json(load(route))
            return
        self.send_json({"message": "not found"}, status=404, envelope=False)

if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", 18080), Handler)
    print("Demo Sub2API: http://127.0.0.1:18080")
    server.serve_forever()
