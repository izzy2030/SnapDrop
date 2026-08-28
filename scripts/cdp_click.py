import json, sys, time, urllib.request

def find_ws():
    targets = json.load(urllib.request.urlopen("http://127.0.0.1:9333/json"))
    for t in targets:
        if t["type"] == "page" and "thumbnail" in t["url"]:
            return t["webSocketDebuggerUrl"]
    raise SystemExit("no thumbnail target")

import websockets.sync.client

ws = websockets.sync.client.connect(find_ws(), max_size=None, open_timeout=10)
_id = [0]
def call(method, params=None):
    _id[0] += 1
    ws.send(json.dumps({"id": _id[0], "method": method, "params": params or {}}))
    while True:
        m = json.loads(ws.recv())
        if m.get("id") == _id[0]:
            return m.get("result", {})

def evals(expr):
    r = call("Runtime.evaluate", {"expression": expr, "returnByValue": True})
    return r.get("result", {}).get("value")

info = evals("(() => { const el = document.querySelector('.thumb-card'); if (!el) return null; const r = el.getBoundingClientRect(); return {x: r.x + r.width/2, y: r.y + r.height/2}; })()")
if not info:
    print("NO CARD")
    sys.exit(1)
x, y = info["x"], info["y"]
call("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x, "y": y})
call("Input.dispatchMouseEvent", {"type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1, "modifiers": 2})
call("Input.dispatchMouseEvent", {"type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1, "modifiers": 2})
print("ctrl+click at", x, y)