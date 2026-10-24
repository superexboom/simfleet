#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Instrument selected CUDA functions for offline, sampled-lane inclusive timing.

Input is split_ptx.py's directory. Output must be a separate research folder;
never package the instrumented module. Read the sf_profile global as pairs of
u64 (calls, inclusive clock cycles), indexed by functions.json. Clear the global
between launches. Only block 0, thread 0 writes it, without atomic contention.
"""
import argparse
import json
import pathlib
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("out", type=pathlib.Path)
    parser.add_argument("--minimum-bytes", type=int, default=100000)
    args = parser.parse_args()
    if args.out.exists() or args.minimum_bytes < 1:
        parser.error("out must be new; minimum-bytes must be positive")
    paths = sorted(args.source.glob("unit-*.ptx"))
    if not paths:
        parser.error("no split PTX units")
    pattern = re.compile(r"(?m)^(?:\.(?:visible|weak)\s+)?\.func\s*(?:\([^)]*\)\s*)?([^\s(]+)\s*\(")
    units, selected = [], []
    for path in paths:
        source = path.read_text(encoding="utf8")
        functions = []
        for match in pattern.finditer(source):
            terminal = re.search("[;{]", source[match.end():])
            at = match.end() + terminal.start()
            if source[at] != "{":
                continue
            depth = 1
            for brace in re.finditer("[{}]", source[at + 1:]):
                depth += 1 if brace[0] == "{" else -1
                if depth == 0:
                    end = at + 1 + brace.end()
                    break
            name = match[1]
            if end - at < args.minimum_bytes and not name.startswith(("_Z13w2c_sf_sf_run", "_Z15sf_make_private")):
                continue
            functions.append((at + 1, end - 1, name))
            selected.append(name)
        units.append((path.name, source, functions))
    if not selected or len(selected) != len(set(selected)):
        raise ValueError("no selected functions or duplicate definitions")
    args.out.mkdir(parents=True)
    declaration = f".visible .global .align 8 .u64 sf_profile[{len(selected) * 2}];\n"
    for unit, (name, source, functions) in enumerate(units):
        for start, end, symbol in reversed(functions):
            index = selected.index(symbol)
            body = source[start:end]
            first = re.search(r"(?m)^\s*(?:[A-Za-z_][\w]*(?:\.[\w]+)*\s+|@)", body)
            if not first:
                raise ValueError("no executable body: " + symbol)
            registers = "\n.reg .b32 %sf_tid, %sf_bid;\n.reg .pred %sf_p, %sf_q;\n.reg .b64 %sf_start, %sf_end, %sf_delta, %sf_ptr, %sf_count;\n"
            entry = """
mov.u32 %sf_tid, %tid.x;
mov.u32 %sf_bid, %ctaid.x;
setp.eq.u32 %sf_p, %sf_tid, 0;
setp.eq.u32 %sf_q, %sf_bid, 0;
and.pred %sf_p, %sf_p, %sf_q;
mov.u64 %sf_start, %clock64;
"""
            leave = f"""mov.u64 %sf_end, %clock64;
sub.u64 %sf_delta, %sf_end, %sf_start;
mov.u64 %sf_ptr, sf_profile;
add.u64 %sf_ptr, %sf_ptr, {index * 16};
@%sf_p ld.global.u64 %sf_count, [%sf_ptr];
add.u64 %sf_count, %sf_count, 1;
@%sf_p st.global.u64 [%sf_ptr], %sf_count;
@%sf_p ld.global.u64 %sf_count, [%sf_ptr+8];
add.u64 %sf_count, %sf_count, %sf_delta;
@%sf_p st.global.u64 [%sf_ptr+8], %sf_count;
"""
            body = registers + body[:first.start()] + entry + body[first.start():]
            body, returns = re.subn(r"(?m)^\s*ret;", lambda _: leave + "ret;", body)
            if not returns:
                raise ValueError("no unconditional return: " + symbol)
            source = source[:start] + body + source[end:]
        header = re.search(r"(?m)^\.address_size[^\n]+\n", source).end()
        global_ = declaration if unit == 0 else declaration.replace(".visible", ".extern")
        source = source[:header] + global_ + source[header:]
        (args.out / name).write_text(source, encoding="utf8", newline="\n")
    (args.out / "functions.json").write_text(json.dumps(selected, indent=2) + "\n", encoding="utf8")
    print(json.dumps(dict(functions=len(selected), sampled_block=0, sampled_thread=0,
                          note="inclusive cycles include children and SIMT waits; do not sum percentages")))


if __name__ == "__main__":
    main()
