#!/usr/bin/env python3
"""Echo 适配器：一个最小的终端通道，用来验证 D44 协议。

不是产品通道：它把核心发来的 View 直接打到终端，把用户输入当成入站文本。
没有原地刷新、没有编辑器，只是流式追加——正适合验证协议。

用法（核心需启用 [[channels.adapter]] id = "echo"）：
    python3 adapters/echo/echo_tui.py --socket /run/chuan/channels/echo.sock

身份：把 `echo:<actor>` 写进 [identity.user] 的 channels；默认 actor 是 `local`。
"""

from __future__ import annotations

import argparse
import curses
import json
import os
import queue
import socket
import sys
import threading
import time
import uuid

PROTOCOL = 1
CAPS = {
    "edit": False,          # Echo 只发终稿，不做原地编辑
    "buttons": True,        # 用编号模拟按钮
    "markdown": False,
    "max_text": 4000,
    "min_edit_interval_ms": 0,
}


class Rpc:
    def __init__(self, sock: socket.socket) -> None:
        self.sock = sock
        self.file = sock.makefile("rwb", buffering=0)
        self.lock = threading.Lock()
        self.next_id = 1

    def send(self, obj: dict) -> None:
        line = (json.dumps(obj, ensure_ascii=False) + "\n").encode()
        with self.lock:
            self.file.write(line)

    def notify(self, method: str, params: dict) -> None:
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def call(self, method: str, params: dict) -> dict:
        with self.lock:
            rid = self.next_id
            self.next_id += 1
            line = (
                json.dumps(
                    {"jsonrpc": "2.0", "id": rid, "method": method, "params": params},
                    ensure_ascii=False,
                )
                + "\n"
            ).encode()
            self.file.write(line)
            raw = self.file.readline()
        if not raw:
            raise ConnectionError("core 关闭了连接")
        msg = json.loads(raw)
        if "error" in msg:
            raise RuntimeError(msg["error"].get("message", "rpc error"))
        return msg.get("result", {})


def block_lines(block: dict) -> list[str]:
    """Block 用 serde 的相邻标签：{"type": "...", "value": ...}。"""
    kind = block.get("type")
    val = block.get("value")
    if kind == "paragraph":
        return str(val or "").splitlines() or [""]
    if kind == "code":
        val = val or {}
        lang = val.get("lang") or ""
        return [f"```{lang}".rstrip(), val.get("text", ""), "```"]
    if kind == "image":
        val = val or {}
        return [f"[图片] {val.get('alt', '')} {val.get('uri', '')}".strip()]
    if kind == "divider":
        return ["─" * 24]
    if kind == "tool_call":
        val = val or {}
        return [
            f"[工具] {val.get('name', '?')} ({val.get('state', '?')}) "
            f"{val.get('preview', '')}".strip()
        ]
    return [json.dumps(block, ensure_ascii=False)]


def view_lines(view: dict) -> list[str]:
    out: list[str] = []
    for block in view.get("blocks", []):
        out.extend(block_lines(block))
    status = view.get("status")
    if status:
        out.append(f"· {status}")
    for row in view.get("buttons", []):
        labels = "  ".join(f"{i + 1}) {b.get('label', '')}" for i, b in enumerate(row))
        if labels:
            out.append(labels)
    return out


class App:
    def __init__(self, rpc: Rpc, channel: str, conversation: str, actor: str) -> None:
        self.rpc = rpc
        self.channel = channel
        self.conversation = conversation
        self.actor = actor
        self.events: queue.Queue = queue.Queue()
        self.lines: list[str] = []
        self.buf = ""
        self.status = "connected"
        self.buttons: list[dict] = []
        self.msg_seq = 0
        self._stop = False

    # ---- 输出 ----
    def add(self, text: str) -> None:
        for line in str(text).splitlines() or [""]:
            self.lines.append(line)

    def drain_events(self) -> None:
        while True:
            try:
                kind, payload = self.events.get_nowait()
            except queue.Empty:
                return
            if kind == "lines":
                for line in payload:
                    self.add(line)
            elif kind == "prompt":
                self.add(payload.get("text", ""))
                self.buttons = [
                    {
                        "label": c.get("label", ""),
                        "data": f"appr:{c.get('id', '')}:{c.get('value', '')}",
                    }
                    for c in payload.get("choices", [])
                ]
            elif kind == "buttons":
                self.buttons = payload
            elif kind == "ack":
                self.add(f"[ack] {payload}")
            elif kind == "status":
                self.status = payload
            elif kind == "disconnected":
                self.status = "core 断开（按 q 退出）"

    # ---- 输入 ----
    def submit(self) -> None:
        text = self.buf.strip()
        self.buf = ""
        if not text:
            return
        if text in ("/quit", "/exit"):
            self._stop = True
            return
        if text.isdigit() and self.buttons:
            idx = int(text) - 1
            if 0 <= idx < len(self.buttons):
                btn = self.buttons[idx]
                self.buttons = []
                self.add(f"> [选择] {btn['label']}")
                self.send(
                    text=btn["data"],
                    kind="callback",
                    callback_id=f"echo-cb-{uuid.uuid4().hex[:8]}",
                )
                return
        self.add(f"> {text}")
        self.send(text=text, kind="text")

    def send(self, text: str, kind: str, callback_id: str | None = None) -> None:
        try:
            self.rpc.notify(
                "chuan.inbound",
                {
                    "conversation": self.conversation,
                    "actor": self.actor,
                    "kind": kind,
                    "text": text,
                    "dedupe_key": uuid.uuid4().hex,
                    "callback_id": callback_id,
                    "msg": None,
                    "attachments": [],
                },
            )
        except OSError as e:
            self.status = f"发送失败：{e}"

    # ---- 绘制 ----
    def draw(self, stdscr) -> None:
        stdscr.erase()
        h, w = stdscr.getmaxyx()
        if h < 5 or w < 20:
            stdscr.addnstr(0, 0, "终端太小", max(0, w - 1))
            stdscr.refresh()
            return

        title = f" chuan echo · {self.channel}:{self.conversation} · {self.status} "
        stdscr.addnstr(0, 0, title.ljust(w - 1), w - 1, curses.A_REVERSE)

        body_h = h - 3
        wrapped: list[str] = []
        for line in self.lines:
            if not line:
                wrapped.append("")
                continue
            while len(line) > w - 2:
                wrapped.append(line[: w - 2])
                line = line[w - 2 :]
            wrapped.append(line)
        visible = wrapped[-body_h:]
        for i, line in enumerate(visible):
            stdscr.addnstr(1 + i, 0, line, w - 1)

        if self.buttons:
            hint = "选择：" + "  ".join(
                f"{i + 1}) {b['label']}" for i, b in enumerate(self.buttons)
            )
            stdscr.addnstr(h - 3, 0, hint[: w - 1], w - 1, curses.A_DIM)

        prompt = "> " + self.buf
        stdscr.addnstr(h - 2, 0, "─" * (w - 1), w - 1, curses.A_DIM)
        stdscr.addnstr(h - 1, 0, prompt[: w - 1], w - 1)
        stdscr.move(h - 1, min(len(prompt), w - 2))
        stdscr.refresh()

    def run(self, stdscr) -> None:
        curses.curs_set(1)
        stdscr.timeout(80)
        while not self._stop:
            self.drain_events()
            self.draw(stdscr)
            try:
                ch = stdscr.get_wch()
            except curses.error:
                continue
            if ch == curses.KEY_RESIZE:
                continue
            if ch in (curses.KEY_ENTER, "\n", "\r"):
                self.submit()
            elif ch in (curses.KEY_BACKSPACE, "\b", "\x7f"):
                self.buf = self.buf[:-1]
            elif ch == "\x03":  # Ctrl-C
                self._stop = True
            elif isinstance(ch, str) and ch.isprintable():
                if len(self.buf) < 4000:
                    self.buf += ch


def reader(app: App, rpc: Rpc, stop: threading.Event) -> None:
    while not stop.is_set():
        try:
            raw = rpc.file.readline()
        except OSError:
            break
        if not raw:
            break
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if "method" not in msg:
            continue
        mid = msg.get("id")
        method = msg.get("method")
        params = msg.get("params") or {}

        def ok(result):
            if mid is not None:
                rpc.send({"jsonrpc": "2.0", "id": mid, "result": result})

        if method == "chuan.deliver":
            app.msg_seq += 1
            mid_str = f"echo-{app.msg_seq}"
            app.events.put(("lines", view_lines(params.get("view") or {})))
            ok({"msg": {"channel": app.channel, "id": mid_str}})
        elif method == "chuan.update":
            # 声明了 edit=false，正常不会走到这里；到了就追加，不假装能编辑。
            app.events.put(("lines", view_lines(params.get("view") or {})))
            ok({"outcome": "edited"})
        elif method == "chuan.ack":
            app.events.put(("ack", params.get("text", "")))
            ok({})
        elif method == "chuan.prompt":
            app.events.put(("prompt", params.get("prompt") or {}))
            app.msg_seq += 1
            ok({"msg": {"channel": app.channel, "id": f"echo-{app.msg_seq}"}})
        elif method == "chuan.send_file":
            app.events.put(
                (
                    "lines",
                    [
                        f"[文件] {params.get('name', '?')} {params.get('path', '')} "
                        f"{params.get('caption', '')}".strip()
                    ],
                )
            )
            app.msg_seq += 1
            ok({"msg": {"channel": app.channel, "id": f"echo-{app.msg_seq}"}})
        elif method == "chuan.clear_buttons":
            app.events.put(("buttons", []))
            ok({})
        else:
            if mid is not None:
                rpc.send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "error": {"code": -32601, "message": f"unknown method {method}"},
                    }
                )
    stop.set()
    app.events.put(("disconnected", None))


def connect(path: str, channel: str, timeout: float = 5.0) -> Rpc:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(timeout)
    sock.connect(path)
    sock.settimeout(None)
    rpc = Rpc(sock)
    rpc.send(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "chuan.hello",
            "params": {
                "protocol": PROTOCOL,
                "channel": channel,
                "adapter_version": "echo-0.1.0",
                "caps": CAPS,
            },
        }
    )
    raw = rpc.file.readline()
    if not raw:
        raise ConnectionError("握手失败：core 关闭了连接")
    reply = json.loads(raw)
    if "error" in reply:
        raise RuntimeError(reply["error"].get("message", "握手被拒"))
    return rpc


def main() -> int:
    ap = argparse.ArgumentParser(description="chuan echo 适配器（D44 协议测试）")
    ap.add_argument("--socket", default=os.environ.get("CHUAN_ECHO_SOCKET", "/run/chuan/channels/echo.sock"))
    ap.add_argument("--channel", default="echo")
    ap.add_argument("--conversation", default="tui")
    ap.add_argument("--actor", default="local")
    args = ap.parse_args()

    try:
        rpc = connect(args.socket, args.channel)
    except (OSError, RuntimeError, ConnectionError) as e:
        print(f"连接 {args.socket} 失败：{e}", file=sys.stderr)
        return 2

    app = App(rpc, args.channel, args.conversation, args.actor)
    app.add("已连接。输入文字回车发送；有选项时输入数字；/quit 退出。")
    stop = threading.Event()
    t = threading.Thread(target=reader, args=(app, rpc, stop), daemon=True)
    t.start()
    try:
        curses.wrapper(app.run)
    finally:
        stop.set()
        try:
            rpc.sock.close()
        except OSError:
            pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
