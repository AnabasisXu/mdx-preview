#!/usr/bin/env python3
"""Smoke-test the multi-dictionary HTTP/WS contract.

usage: smoke.py <prev binary> <sample.mdx>

Copies the sample dictionary into two files plus one truncated file, starts
prev on a free port, and exercises the endpoints and WebSocket methods.

Exits 0 on success, 1 with a message on the first failed expectation.
"""

import base64
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def get(url, expect=200):
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            body = r.read()
            code = r.status
    except urllib.error.HTTPError as e:
        body = e.read()
        code = e.code
    if code != expect:
        raise AssertionError(f"{url}: expected {expect}, got {code} ({body[:80]!r})")
    return body


def ws_open(port):
    key = base64.b64encode(os.urandom(16)).decode()
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    s.sendall(
        (
            f"GET /mdx-preview/api/v1/ws/ HTTP/1.1\r\nHost: 127.0.0.1\r\n"
            f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    head = b""
    while b"\r\n\r\n" not in head:
        head += s.recv(4096)
    assert b" 101 " in head.split(b"\r\n")[0], head.split(b"\r\n")[0]
    return s


def ws_send(s, payload):
    data = json.dumps(payload).encode()
    n = len(data)
    if n < 126:
        header = bytes([0x81, 0x80 | n])
    elif n < 65536:
        header = bytes([0x81, 0x80 | 126]) + struct.pack(">H", n)
    else:
        header = bytes([0x81, 0x80 | 127]) + struct.pack(">Q", n)
    mask = os.urandom(4)
    s.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))


def ws_recv(s):
    def read(n):
        buf = b""
        while len(buf) < n:
            chunk = s.recv(n - len(buf))
            if not chunk:
                raise AssertionError("websocket closed")
            buf += chunk
        return buf

    head = read(2)
    length = head[1] & 0x7F
    if length == 126:
        length = struct.unpack(">H", read(2))[0]
    elif length == 127:
        length = struct.unpack(">Q", read(8))[0]
    return json.loads(read(length))


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__.strip())
    binary, sample = sys.argv[1], sys.argv[2]
    port = free_port()
    work = tempfile.mkdtemp(prefix="prev-smoke-")
    try:
        os.makedirs(os.path.join(work, "one"))
        os.makedirs(os.path.join(work, "two"))
        shutil.copy(sample, os.path.join(work, "one", "alpha.mdx"))
        shutil.copy(sample, os.path.join(work, "two", "beta.mdx"))
        with open(sample, "rb") as f:
            head = f.read(200)
        with open(os.path.join(work, "broken.mdx"), "wb") as f:
            f.write(head)

        proc = subprocess.Popen(
            [binary, work, "--port", str(port)],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        base = f"http://127.0.0.1:{port}/mdx-preview"
        deadline = time.time() + 10
        while True:
            try:
                parser = json.loads(get(base + "/api/parser"))
                break
            except Exception:
                if time.time() > deadline:
                    raise
                time.sleep(0.02)

        dicts = parser["dicts"]
        names = [d["name"] for d in dicts]
        assert names == ["alpha", "beta"], names
        assert all(d["count"] > 0 for d in dicts), dicts
        assert dicts[0]["count"] == dicts[1]["count"], dicts

        for i in range(len(dicts)):
            html = get(f"{base}/api/d/{i}/item/1").decode()
            want = f'<base href="/mdx-preview/api/d/{i}/res/" />'
            assert want in html, html[:300]
            hits = json.loads(get(f"{base}/api/d/{i}/lookup/ab"))
            assert hits["items"], hits

        get(f"{base}/api/d/9/item/1", expect=404)
        get(f"{base}/api/d/9/lookup/ab", expect=404)
        get(f"{base}/api/d/0/res/missing.png", expect=404)
        get(f"http://127.0.0.1:{port}/favicon.ico", expect=204)

        # The shell must not be cached, or an upgraded binary keeps serving the
        # previous protocol's URLs to an already-open tab.
        shell = base + "/"
        for url in (shell, base + "/dist/utils.js", base + "/dist/site.css"):
            with urllib.request.urlopen(url) as r:
                cache = r.headers.get("Cache-Control", "")
                assert "no-store" in cache, (url, cache)

        s = ws_open(port)
        try:
            ws_send(s, {"id": 1, "method": "suggest_words", "dict": 0, "params": ["", 0, False]})
            reply = ws_recv(s)
            assert reply["result"]["items"], reply
            ws_send(s, {"id": 2, "method": "suggest_words", "dict": 1, "params": ["ab", 0, False]})
            reply = ws_recv(s)
            assert reply["result"]["items"], reply
            ws_send(s, {"id": 3, "method": "random", "dict": 0, "params": []})
            reply = ws_recv(s)
            assert reply["result"]["word"] and reply["result"]["k_id"] >= 1, reply
            ws_send(s, {"id": 4, "method": "initials", "dict": 1, "params": []})
            reply = ws_recv(s)
            assert reply["result"], reply
            ws_send(s, {"id": 5, "method": "suggest_words", "dict": 9, "params": ["", 0, False]})
            reply = ws_recv(s)
            assert reply["error"] == "unknown dictionary", reply
            ws_send(s, {"id": 6, "method": "suggest_words", "params": ["", 0, False]})
            reply = ws_recv(s)
            assert reply["error"] == "unknown dictionary", reply
        finally:
            s.close()

        # A redirect broadcast must name the dictionary it came from.
        listener = ws_open(port)
        sender = ws_open(port)
        try:
            ws_send(sender, {"id": 7, "method": "redirect", "dict": 1, "params": [42]})
            ws_recv(sender)
            event = ws_recv(listener)
            assert event.get("event") == "redirect", event
            assert event.get("dict") == 1, event
            assert event.get("params") == [42], event
        finally:
            listener.close()
            sender.close()

        proc.terminate()
        try:
            out = proc.communicate(timeout=5)[0] or ""
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.communicate()
            raise AssertionError("server did not exit within 5 s of SIGTERM")
        assert "2 dictionaries" in out, out
        assert "skipping" in out, out
        print("ok: 2 dictionaries, HTTP and WS contract verified")
    finally:
        if "proc" in locals() and proc.poll() is None:
            proc.kill()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
