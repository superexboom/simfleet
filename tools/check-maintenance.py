#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Exercise memory replacement against a local protocol-8 server.

python tools/check-maintenance.py --exe target/release/simfleet.exe \
    --assets <directory containing cache/ and engines/> --engine <id> \
    --wasm <id> --data <id> --order <board-order.json> --out <new directory>

Requires psutil. Uses synthetic ids and no account; no work is requested from
WFSim. The real, pinned engine executes a small pack on each new process.
"""
import argparse
import datetime
import hashlib
import json
import pathlib
import shutil
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import psutil


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("exe", "assets", "order", "out"):
        p.add_argument("--" + name, type=pathlib.Path, required=True)
    for name in ("engine", "wasm", "data"):
        p.add_argument("--" + name, required=True)
    p.add_argument("--cycles", type=int, default=4)
    p.add_argument("--no-watch", action="store_true")
    p.add_argument("--fail-save", action="store_true", help="temporarily make kept/ unwritable; verify retry before replacement")
    p.add_argument("--lanes", type=int, default=2)
    p.add_argument("--placement", choices=["auto","system","numa","cache"], default="auto")
    args = p.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    exe = args.out / args.exe.name
    shutil.copy2(args.exe, exe)
    for name in ("cache", "engines"):
        shutil.copytree(args.assets / name, args.out / name)
    for filename, prefix in [(f"wfsim_wasm_bg.{args.wasm}.wasm", args.wasm), (f"pack.data.{args.data}.json", args.data)]:
        source=args.out/'cache'/filename
        if not source.is_file() or not hashlib.sha256(source.read_bytes()).hexdigest().startswith(prefix):
            raise ValueError(f"Missing or mismatched reference asset: {source}")
    order = json.loads(args.order.read_text(encoding="utf-8"))
    requests, received, handed, lock = [], {}, {}, threading.Lock()
    release = dict(engine=args.engine, wasm=args.wasm, data=args.data, protocol=8, protocols_served=[8, 7])

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def answer(self, value):
            body = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/release.json":
                self.answer(release)
            elif self.path == "/work.json":
                self.answer({"v": 1, "at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
                             "engine": args.engine, "open": 100, "urgent": 1,
                             "poll_ms": 1000, "jitter_ms": 0, "urgent_jitter_ms": 0})
            else:
                self.answer({"ok": True})

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
            if self.path != "/api/board/sync":
                self.answer({"ok": True})
                return
            with lock:
                requests.append(body)
                results = body.get("results", [])
                taken = []
                for r in results:
                    q = r["question"]
                    result = {k: r[k] for k in ("score", "metric", "work", "counts")}
                    if q in received and received[q] != result:
                        raise AssertionError("changed answer")
                    received[q] = result
                    taken.append(q)
                # Each new process has pace=0 only on its first ever ask, so
                # identify a new pack by each actual request for work instead.
                pack = None
                if body.get("want_ms", 0) > 0:
                    number = len(handed) + 1
                    lease = f"local-{number}"
                    tasks = [{**order, "question": f"local-{number}-{i}", "lease": lease} for i in range(12)]
                    handed[number] = [t["question"] for t in tasks]
                    pack = {"lease": lease, "lease_ms": 1500000, "tasks": tasks}
                reply = {"ok": True, "engine": args.engine, "protocol": 8, "protocols_served": [8, 7],
                         "taken": taken, "dropped": [], "states": [], "held": [], "next_ms": 600000, "pack": pack}
            self.answer(reply)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = f'''server = "http://127.0.0.1:{server.server_port}"
verifier = "0123456789abcdef01234567"
consent_at = "2026-10-21T00:00:00.000Z"
consent_v = 3
idle_cores = {args.lanes}
active_cores = {args.lanes}
idle_after_secs = 60
pause_on_battery = false
priority = "normal"
placement = "{args.placement}"
native_engine = true
native_engine_url = ""
official_engine = false
native_riven = false
slots = 1
max_slots = 1
protocol = 8
watch = true
memory_limit_mb = 1
'''
    (args.out / "simfleet.toml").write_text(config, encoding="utf-8")
    obstruction = args.out / "kept"
    if args.fail_save:
        obstruction.write_text("owned by this test; blocks creation of kept/", encoding="utf-8")
    log_path = args.out / "client.log"
    started, samples, known = time.monotonic(), [], {}
    process = None
    save_failed_at, save_recovered = None, False
    try:
        with log_path.open("wb") as log:
            command = [str(exe.resolve()), "run"] + (["--no-watch"] if args.no_watch else [])
            process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
            while time.monotonic() - started < 240:
                candidates = []
                for proc in psutil.process_iter(["pid", "exe"]):
                    try:
                        if proc.info["exe"] and pathlib.Path(proc.info["exe"]).resolve() == exe.resolve():
                            candidates.append(proc)
                    except (psutil.Error, OSError):
                        continue
                for proc in candidates:
                    try:
                        m = proc.memory_info()
                        row = dict(seconds=round(time.monotonic()-started, 2), pid=proc.pid,
                                   private_bytes=getattr(m, "private", m.rss), rss_bytes=m.rss,
                                   threads=proc.num_threads())
                        samples.append(row)
                        known[proc.pid] = proc
                    except psutil.Error:
                        pass
                text = log_path.read_text(encoding="utf-8", errors="replace")
                if not args.no_watch and process.poll() is not None:
                    raise AssertionError(f"client exited early: {process.returncode}\n{text[-3000:]}")
                replaced = text.count("旧进程已退出") if not args.no_watch else text.count("simfleet 0.") - 1
                if args.fail_save and not save_recovered and "无法把结果写进" in text and "[内存]" in text:
                    assert replaced == 0, "replaced before an answer could be saved"
                    if save_failed_at is None:
                        save_failed_at = time.monotonic()
                    if time.monotonic() - save_failed_at >= 3:
                        obstruction.unlink()
                        save_recovered = True
                if replaced >= args.cycles:
                    break
                time.sleep(0.25)
            else:
                raise AssertionError("replacement did not finish before timeout")
            with lock:
                complete = [n for n, questions in handed.items() if all(q in received for q in questions)]
                assert len(complete) >= args.cycles, (complete, handed, received)
                answers = list(received.values())
                assert answers and all(a == answers[0] for a in answers)
                assert not args.fail_save or save_recovered
                assert "意外退出" not in text and "失败" not in text, text[-3000:]
                assert not (args.out / "simfleet-crash.log").exists()
                summary = dict(replacements=replaced, complete_packs=len(complete), results=len(received), save_recovered=save_recovered,
                               distinct_pids=len(known), answer=answers[0],
                               max_private_bytes=max(r["private_bytes"] for r in samples),
                               exe_sha256=hashlib.sha256(exe.read_bytes()).hexdigest())
                (args.out / "result.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
                print(json.dumps(summary), flush=True)
    finally:
        # Only this harness's copied executable, including an unwatched successor.
        for proc in psutil.process_iter(["exe"]):
            try:
                if proc.info["exe"] and pathlib.Path(proc.info["exe"]).resolve() == exe.resolve():
                    proc.terminate()
                    proc.wait(timeout=10)
            except (psutil.Error, OSError):
                pass
        if process:
            process.wait(timeout=10)
        server.shutdown()
        server.server_close()
        (args.out / "memory.json").write_text(json.dumps(samples), encoding="utf-8")
        (args.out / "requests.json").write_text(json.dumps(requests, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
