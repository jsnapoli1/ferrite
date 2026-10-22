"""Stand-in for an ArtCraft `<app>-cli mcp` server, used by Ferrite's tests.

Like the real Craft CLIs it speaks the legacy MCP handshake (2025-06-18) over stdio and
confines itself to --automation-read-root / --automation-write-root folders.
"""
import json
import os
import sys

args = sys.argv[1:]
assert args and args[0] == "mcp", args
roots = [args[i + 1] for i, a in enumerate(args) if a in ("--automation-read-root", "--automation-write-root")]
root = os.path.realpath(roots[0]) if roots else os.getcwd()
state = {"path": None, "text": ""}

TOOLS = [
    {"name": "open_document", "description": "Open a document", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
    {"name": "type_text", "description": "Append text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "save_document", "description": "Save", "inputSchema": {"type": "object"}},
    {"name": "export_pdf", "description": "Export a PDF next to the document", "inputSchema": {"type": "object"}},
]


def inside(p):
    p = os.path.realpath(p if os.path.isabs(p) else os.path.join(root, p))
    if not p.startswith(root + os.sep):
        raise ValueError("path outside the automation root")
    return p


def call(name, a):
    if name == "open_document":
        state["path"] = inside(a["path"])
        with open(state["path"], encoding="utf-8", errors="replace") as f:
            state["text"] = f.read()
        return "opened " + os.path.basename(state["path"])
    if name == "type_text":
        state["text"] += a["text"]
        return "ok"
    if name == "save_document":
        with open(state["path"], "w", encoding="utf-8") as f:
            f.write(state["text"])
        return "saved"
    if name == "export_pdf":
        out = os.path.splitext(state["path"])[0] + ".pdf"
        with open(out, "wb") as f:
            f.write(b"%PDF-1.7 fake")
        return "exported " + os.path.basename(out)
    raise KeyError(name)


initialized = False
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    mid, method = msg["id"], msg.get("method")
    if method == "initialize":
        initialized = True
        res = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "wordcraft", "version": "0.0.0-test"}}
        out = {"jsonrpc": "2.0", "id": mid, "result": res}
    elif not initialized:
        out = {"jsonrpc": "2.0", "id": mid, "error": {"code": -32002, "message": "server not initialized"}}
    elif method == "tools/list":
        out = {"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}}
    elif method == "tools/call":
        p = msg["params"]
        try:
            text = call(p["name"], p.get("arguments", {}))
            out = {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": text}]}}
        except Exception as e:  # noqa: BLE001 - report every failure as a tool error
            out = {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": str(e)}], "isError": True}}
    else:
        out = {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown method"}}
    sys.stdout.write(json.dumps(out) + "\n")
    sys.stdout.flush()
