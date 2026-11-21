#!/usr/bin/env python3
"""Command Code transport — bridges Rust gateway to /alpha/generate.

Reads JSON request from stdin: {url, token, body}
Streams NDJSON response lines to stdout.
Errors go to stderr as JSON: {"error": "...", "status": N}

Uses urllib (not requests/httpx) to match CLI TLS fingerprint —
reqwest/rustls triggers Command Code's proxy detection.
"""
import sys
import json
import urllib.request
import urllib.error


def main():
    raw = sys.stdin.buffer.read()
    if not raw:
        print(json.dumps({"error": "no input", "status": 400}), file=sys.stderr)
        sys.exit(1)

    req_data = json.loads(raw.decode("utf-8"))
    url = req_data["url"]
    token = req_data["token"]
    body = json.dumps(req_data["body"]).encode("utf-8")

    headers = req_data.get("headers", {})
    if not headers:
        headers = {
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "User-Agent": "cli",
            "x-command-code-version": "1.87.1",
            "x-cli-environment": "production",
            "x-taste-learning": "false",
            "x-co-flag": "false",
            "x-session-id": req_data.get("session_id", ""),
        }

    req = urllib.request.Request(url, data=body)
    for k, v in headers.items():
        req.add_header(k, v)

    try:
        resp = urllib.request.urlopen(req, timeout=300)
        # Stream NDJSON lines to stdout
        for line in resp:
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
    except urllib.error.HTTPError as e:
        body_text = ""
        try:
            body_text = e.read().decode("utf-8", "replace")[:500]
        except Exception:
            pass
        print(json.dumps({"error": body_text or str(e.reason), "status": e.code}), file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(json.dumps({"error": str(e), "status": 0}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
