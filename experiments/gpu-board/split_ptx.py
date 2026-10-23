#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Split generated PTX by function to bound assembler memory consumption."""
import argparse
import hashlib
import json
import pathlib
import re


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("source", type=pathlib.Path)
    p.add_argument("out", type=pathlib.Path)
    p.add_argument("--unit-kib", type=int, default=512)
    args = p.parse_args()
    source = args.source.read_text()
    header = "\n".join(re.findall(r"^\.(?:version|target|address_size)[^\n]+", source, re.M)) + "\n"
    functions, prototypes = [], {}
    pattern = re.compile(r"^(?:\.(?:visible|weak|extern)\s+)?\.(func|entry)\b", re.M)
    pos = 0
    while found := pattern.search(source, pos):
        terminal = re.search(r"[;{]", source[found.end():])
        at = found.end() + terminal.start()
        signature = source[found.start():at].strip()
        name = re.search(r"\.(?:func|entry)\s*(?:\([^)]*\)\s*)?([^\s(]+)", signature)[1]
        pos = at + 1
        if source[at] == ";":
            if found[1] == "func":
                prototypes[name] = re.sub(r"^\.(?:visible|weak|extern)\s+", "", signature)
            continue
        depth = 1
        for brace in re.finditer(r"[{}]", source[pos:]):
            depth += 1 if brace[0] == "{" else -1
            if depth == 0:
                end = pos + brace.end()
                break
        body = source[at:end]
        signature = re.sub(r"^\.(?:visible|weak|extern)\s+", "", signature)
        functions.append((name, found[1], signature, body))
        if found[1] == "func":
            prototypes[name] = signature
        pos = end
    globals_ = re.findall(r"^(?:\.(?:visible|weak)\s+)?\.(?:global|const)\b[^;]+;", source, re.M)
    globals_ = [re.sub(r"^\.(?:visible|weak)\s+", "", item) for item in globals_]
    external_globals = "\n".join(".extern " + item.split("=", 1)[0].rstrip().removesuffix(";") + ";" for item in globals_)
    chunks, chunk, size = [], [], 0
    for function in functions:
        if chunk and size + len(function[3]) > args.unit_kib * 1024:
            chunks.append(chunk); chunk = []; size = 0
        chunk.append(function); size += len(function[3])
    if chunk: chunks.append(chunk)
    args.out.mkdir(parents=True, exist_ok=True)
    records = []
    for number, chunk in enumerate(chunks):
        own = {f[0] for f in chunk}
        bodies = "\n".join(".visible " + signature + "\n" + body for _, _, signature, body in chunk)
        tokens = set(re.findall(r"[A-Za-z_$][A-Za-z0-9_$]*", bodies))
        dependencies = sorted(tokens.intersection(prototypes).difference(own))
        prefix = "\n".join(".extern " + prototypes[n] + ";" for n in dependencies)
        prefix += "\n" + "\n".join(".visible " + signature + ";" for _, kind, signature, _ in chunk if kind == "func")
        if number == 0:
            data = "\n".join(".visible " + item for item in globals_)
        else:
            data = external_globals
        text = header + prefix + "\n" + data + "\n" + bodies + "\n"
        filename = f"unit-{number:03d}.ptx"
        (args.out / filename).write_text(text, newline="\n")
        records.append(dict(file=filename, functions=[f[0] for f in chunk], bytes=len(text), sha256=hashlib.sha256(text.encode()).hexdigest()))
    manifest = dict(source_sha256=hashlib.sha256(args.source.read_bytes()).hexdigest(), functions=len(functions), globals=len(globals_), units=records)
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2))
    print(json.dumps(dict(functions=len(functions), globals=len(globals_), units=len(chunks), largest=max(r["bytes"] for r in records))))


if __name__ == "__main__": main()
