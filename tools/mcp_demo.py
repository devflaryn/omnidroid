#!/usr/bin/env python3
"""Drive the real MCP server (`target/release/omni-mcp`) over stdio as an agent would, timing each
tool call.

    python tools/mcp_demo.py [--out calls.jsonl] STEP...

    start:<apk>        start_instance {apk}         (installed unless the device holds these bytes)
    install:<apk>      install_apk {apk}
    stop               stop_instance {the last instance_id}
    shell:<command>    shell {command}
    shot:<file.png>    screenshot of the last instance, copied to <file.png>
    status             device_status
    stopdevice         stop_device
    sleep:<secs>

Each call prints one JSON line: the step, the wall seconds the call took, and the tool's answer.
The server gets this process's environment (OMNIDROID_DYNARMIC_BUILD_DIR, OMNI_MCP_*).
"""
import json, os, shutil, subprocess, sys, time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def main():
    args = sys.argv[1:]
    out = None
    if args[:1] == ["--out"]:
        out, args = args[1], args[2:]
    exe = REPO / "target" / "release" / ("omni-mcp.exe" if os.name == "nt" else "omni-mcp")
    server = subprocess.Popen([str(exe)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1, cwd=REPO)
    ids = iter(range(1, 10**6))

    def rpc(method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        if not notify:
            msg["id"] = next(ids)
        server.stdin.write(json.dumps(msg) + "\n")
        server.stdin.flush()
        if notify:
            return None
        while True:
            line = server.stdout.readline()
            if not line:
                raise SystemExit("the server ended")
            reply = json.loads(line)
            if reply.get("id") == msg["id"]:
                return reply

    def call(name, arguments):
        reply = rpc("tools/call", {"name": name, "arguments": arguments})
        if "error" in reply:
            return {"error": reply["error"]}
        text = reply["result"]["content"][0]["text"]
        try:
            return json.loads(text)
        except ValueError:
            return {"text": text}

    rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "mcp_demo", "version": "1"}})
    rpc("notifications/initialized", notify=True)
    last = None
    for step in args:
        kind, _, arg = step.partition(":")
        t = time.time()
        if kind == "start":
            answer = call("start_instance", {"apk": arg})
            last = answer.get("instance_id", last)
        elif kind == "install":
            answer = call("install_apk", {"apk": arg})
        elif kind == "stop":
            answer = call("stop_instance", {"instance_id": last})
        elif kind == "shell":
            answer = call("shell", {"command": arg})
        elif kind == "shot":
            answer = call("screenshot", {"instance_id": last} if last else {})
            if "path" in answer:
                shutil.copy(answer["path"], arg)
        elif kind == "status":
            answer = call("device_status", {})
        elif kind == "stopdevice":
            answer = call("stop_device", {})
        elif kind == "sleep":
            time.sleep(float(arg))
            answer = {}
        else:
            raise SystemExit(f"unknown step {step}")
        row = {"step": step, "call_s": round(time.time() - t, 3), "at": time.strftime("%H:%M:%S"), "answer": answer}
        print(json.dumps(row), flush=True)
        if out:
            with open(out, "a") as f:
                f.write(json.dumps(row) + "\n")
    server.stdin.close()
    server.wait(timeout=120)


if __name__ == "__main__":
    main()
