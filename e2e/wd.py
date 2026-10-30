"""Minimal W3C WebDriver client for driving the Simple Setup desktop app through tauri-driver.
Standard library only."""
import base64, json, os, time, urllib.request

B = os.environ.get("WEBDRIVER_URL", "http://127.0.0.1:4444")
TAB, ENTER, ESCAPE, SHIFT = "", "", "", ""


class Session:
    def __init__(self, app, shots):
        self.shots = shots
        os.makedirs(shots, exist_ok=True)
        caps = {"capabilities": {"alwaysMatch": {"tauri:options": {"application": app}}}}
        self.sid = self.req("POST", "/session", caps)["value"]["sessionId"]
        self.s = f"/session/{self.sid}"

    def req(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        r = urllib.request.Request(B + path, data=data, method=method, headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(r, timeout=600) as resp:
            return json.loads(resp.read() or b"{}")

    def close(self):
        self.req("DELETE", self.s)

    def els(self, xpath):
        found = self.req("POST", self.s + "/elements", {"using": "xpath", "value": xpath})["value"]
        return [list(e.values())[0] for e in found]

    def shot(self, name):
        png = base64.b64decode(self.req("GET", self.s + "/screenshot")["value"])
        open(os.path.join(self.shots, f"{name}.png"), "wb").write(png)

    def wait(self, xpath, timeout=60):
        end = time.time() + timeout
        while time.time() < end:
            found = self.els(xpath)
            if found:
                return found[0]
            time.sleep(0.3)
        self.shot("timeout")
        raise SystemExit(f"timed out waiting for {xpath}")

    def click(self, xpath, timeout=30):
        self.req("POST", f"{self.s}/element/{self.wait(xpath, timeout)}/click", {})
        time.sleep(0.4)

    def type_into(self, xpath, text):
        e = self.wait(xpath)
        self.req("POST", f"{self.s}/element/{e}/clear", {})
        self.req("POST", f"{self.s}/element/{e}/value", {"text": text})
        time.sleep(0.3)

    def text(self, xpath):
        return self.req("GET", f"{self.s}/element/{self.wait(xpath)}/text")["value"]

    def js(self, script, *args):
        return self.req("POST", self.s + "/execute/sync", {"script": script, "args": list(args)})["value"]

    def js_async(self, script, *args):
        return self.req("POST", self.s + "/execute/async", {"script": script, "args": list(args)})["value"]

    def keys(self, *keys):
        acts = []
        for k in keys:
            acts += [{"type": "keyDown", "value": k}, {"type": "keyUp", "value": k}]
        self.req("POST", self.s + "/actions", {"actions": [{"type": "key", "id": "kb", "actions": acts}]})
        self.req("DELETE", self.s + "/actions")
        time.sleep(0.2)

    def window(self, width, height):
        self.req("POST", self.s + "/window/rect", {"width": width, "height": height})
        time.sleep(0.5)
