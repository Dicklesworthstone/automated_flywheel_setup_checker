#!/bin/bash
# E2E: a failing run creates a GitHub issue through a mock API (python http.server), and
# `notify --last-run` re-sends. The token never appears in verbose logs.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "github_notification"
ensure_binary
command -v python3 > /dev/null 2>&1 || skip_test "python3 not available for the mock API"

# Mock GitHub API: GET issues -> [], POST issues -> 201 #7; every request appended to a log.
cat > "$TEST_TMP/mock_api.py" <<'EOF'
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
log = sys.argv[2]
class H(BaseHTTPRequestHandler):
    def _record(self, body):
        with open(log, "a") as f:
            f.write(json.dumps({"method": self.command, "path": self.path, "auth": self.headers.get("Authorization", ""), "body": body}) + "\n")
    def do_GET(self):
        self._record("")
        data = b"[]"
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0")); body = self.rfile.read(n).decode()
        self._record(body)
        data = json.dumps({"number": 7, "html_url": "http://mock/7"}).encode()
        self.send_response(201); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
EOF
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 "$TEST_TMP/mock_api.py" "$port" "$TEST_TMP/output/api.log" &
api_pid=$!
trap 'kill $api_pid 2>/dev/null || true; cleanup_test' EXIT
wait_for "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", $port), timeout=1)'" 10

write_config "[notifications]
enabled = true
mode = \"every_run\"
github_token_env = \"E2E_GH_TOKEN\"
github_issue_repo = \"acme/flywheel\"
github_api_url = \"http://127.0.0.1:$port\""
add_dependency_failure "broken_tool" > /dev/null

token="ghp_e2eSECRET0123456789abcdefghijklmnop"
code=0
E2E_GH_TOKEN="$token" run_checker check --local --format jsonl -vvv > "$TEST_TMP/output/run.jsonl" 2> "$TEST_TMP/output/run.err" || code=$?
assert_eq "$code" "1" "failure exit"
assert_file_exists "$TEST_TMP/output/api.log" "the API was called"
assert_eq "$(jq -r 'select(.method == "POST" and .path == "/repos/acme/flywheel/issues") | .path' "$TEST_TMP/output/api.log" | wc -l | tr -d ' ')" "1" "one issue created"
created=$(jq -r 'select(.method == "POST") | .body' "$TEST_TMP/output/api.log" | head -1)
assert_contains "$(echo "$created" | jq -r .title)" "AFSC canary" "issue title"
assert_contains "$(echo "$created" | jq -r .body)" "broken_tool" "issue body lists the failure"
assert_contains "$(echo "$created" | jq -r '.labels | join(",")')" "afsc-automated" "label"
assert_eq "$(jq -r '.auth' "$TEST_TMP/output/api.log" | head -1)" "Bearer $token" "token sent as a bearer header"
assert_not_matches "$(cat "$TEST_TMP/output/run.err")" "$token" "token never logged"

# Explicit re-send for the persisted run.
resend=$(E2E_GH_TOKEN="$token" run_checker notify --last-run --format json 2> /dev/null)
assert_eq "$(echo "$resend" | jq -r .decision)" "sent" "notify --last-run sends"
assert_eq "$(echo "$resend" | jq -r .github)" "created" "created again (mock lists no open issues)"
echo "github_notification: PASSED"
