#!/usr/bin/env python3
"""Compare complete quick-fleet transcripts and repeated fresh search lifetimes.

python experiments/check-lead-regression.py old.dll new.dll --out results
Runs locally; never contacts a task server. Windows private commit includes Python.
"""
import argparse
import copy
import ctypes as c
import gc
import hashlib
import importlib.util
import json
import math
import pathlib
import sys
from concurrent.futures import ThreadPoolExecutor

sys.dont_write_bytecode = True


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


class Native:
    def __init__(self, path):
        self.lib = c.CDLL(str(path.resolve()))
        self.fn = self.lib.sfe_optimize
        self.fn.argtypes = [c.c_char_p, c.c_size_t, c.POINTER(c.c_size_t)]
        self.fn.restype = c.c_void_p
        self.free = self.lib.sfe_free
        self.free.argtypes = [c.c_void_p, c.c_size_t]

    def call(self, req, fleet):
        body = json.dumps({**req, "quick_fleet": fleet}).encode()
        size = c.c_size_t()
        ptr = self.fn(body, len(body), c.byref(size))
        if not ptr:
            raise RuntimeError("native panic")
        try:
            out = json.loads(c.string_at(ptr, size.value))
        finally:
            self.free(ptr, size.value)
        if out.get("ok") is False:
            raise RuntimeError(out)
        return out


def search(lib, req, pool, lanes=6):
    scores, trace, work, fights = [], [], 0, 0
    for step in range(2000):
        out = lib.call(req, {"lead": True, "fresh": step == 0, "scores": scores})
        if "pending" not in out:
            return {"answer": out, "steps": step + 1, "trace": trace,
                    "work": work + out.get("final_work", 0), "fights": fights}
        pending = out["pending"]
        size = max(1, math.ceil(len(pending) / (2 * lanes)))
        parts = [pending[i:i + size] for i in range(0, len(pending), size)]
        outs = list(pool.map(lambda part: lib.call(req, {"score": part}), parts))
        trace.append([digest(out), digest(outs)])
        scores = [s for o in outs for s in o.get("scores", [])]
        work += sum(o.get("work", 0) for o in outs)
        fights += sum(o.get("fights", 0) for o in outs)
    raise RuntimeError("search did not settle")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("baseline", type=pathlib.Path)
    p.add_argument("candidate", type=pathlib.Path)
    p.add_argument("--out", type=pathlib.Path, required=True)
    p.add_argument("--cycles", type=int, default=40)
    a = p.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    old, new = Native(a.baseline), Native(a.candidate)
    start = lambda card: {"slots": [card], "evolutions": [], "arcane": [],
                          "fixed": [{"kind": "mods", "idx": 0}]}
    req = {"weapon": "braton_prime", "enemy": "thrax_centurion", "level": 100,
           "duration": 2, "runs": 10, "candidate_runs": 10, "finalists": 3,
           "strategy": "quick", "build_size": 4, "build_min": 0,
           "mods": {x: "search" for x in ["serration", "split_chamber", "point_strike", "vital_sense",
                                          "hellfire", "infected_clip", "stormbringer", "cryo_rounds"]},
           "exilus": {"none": "fixed"},
           "arcanes": {"primary_deadhead": "search", "primary_merciless": "search"},
           "starts": [start("hellfire"), start("infected_clip")]}
    cases = [("screened-multi", req)]
    cases.append(("unscreened-multi", {**req, "candidate_runs": 1}))
    cases.append(("budget-cut", {**req, "max_evals": 20}))
    riven = copy.deepcopy(req)
    riven["weapon"] = "torid"
    riven["mods"]["riven:Probe"] = "search"
    riven["rivens"] = [{"id": "Probe", "spec": {"bonuses": [{"id": "multishot", "roll": 1.1}],
                                                 "rank": 8, "polarity": "madurai"}}]
    riven["starts"] = [start("riven:Probe"), start("hellfire")]
    cases.append(("torid-riven", riven))
    reroll = copy.deepcopy(riven)
    reroll["rivens"][0]["spec"]["bonuses"][0]["roll"] = 0.9
    cases.append(("same-id-new-roll", reroll))
    cases.append(("return-to-braton", req))
    expected = []
    with ThreadPoolExecutor(max_workers=6) as pool:
        for name, body in cases:
            b = search(old, body, pool)
            n = search(new, body, pool)
            (a.out / (name + ".json")).write_text(json.dumps({"request": body, "baseline": b, "candidate": n}), encoding="utf-8")
            assert n == b, name
            expected.append(b)
            print(f"MATCH {name}: {n['steps']} steps, {n['fights']} fights, work={n['work']}", flush=True)

    spec = importlib.util.spec_from_file_location("probe_memory", pathlib.Path(__file__).resolve().parents[1] / "tools/probe-native-memory.py")
    probe = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(probe)
    memory = []
    # Same leader and scorers across changing requests, then retire all lanes.
    def life():
        with ThreadPoolExecutor(max_workers=6) as pool:
            for i in range(a.cycles):
                at = i % len(cases)
                actual = search(new, cases[at][1], pool)
                assert actual == expected[at], ("fresh state", i)
                gc.collect()
                memory.append({"cycle": i + 1, **probe.memory()})
                if i % 10 == 9:
                    print(json.dumps(memory[-1]), flush=True)
    with ThreadPoolExecutor(max_workers=1) as leader:
        leader.submit(life).result()
    memory.append({"phase": "lanes-retired", **probe.memory()})
    (a.out / "memory.json").write_text(json.dumps(memory, indent=2), encoding="utf-8")
    print(json.dumps(memory[-1]), flush=True)


if __name__ == "__main__":
    main()
