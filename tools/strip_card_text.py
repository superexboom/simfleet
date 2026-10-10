# SPDX-License-Identifier: AGPL-3.0-or-later
"""Blank Digital Extremes' card text in a WFSim checkout before a native build.

WFSim's LICENSE-DATA.md §4: card text (`description:` lines and the localized
card text under data/i18n/) is Digital Extremes' copyright and WFSim makes no
grant for it. A native engine that is published must therefore not carry it.
The engine only DISPLAYS that text (it fills a card's X% placeholders); no fight
reads it, so blanking it moves no number — and the build that is published is
held bit for bit against the served wasm before it is (tools/check, CI).

Line-based on purpose: WFSim's loaders read some YAML as lines, so values are
replaced in place and every other byte is kept.

    python tools/strip_card_text.py <wfsim checkout>
"""
import pathlib
import re
import sys

DESC = re.compile(r'^(\s*(?:-\s+)?)description:(.*)$')
ITEM = re.compile(r'^(\s+)-\s+(".*"|\'.*\')\s*$')
KEYVAL = re.compile(r'^(\s+)([A-Za-z0-9_]+):\s*(.+)$')


def indent(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def quoted_end(lines, i, start):
    """Index of the line where the quoted scalar opened at lines[i][start] closes."""
    q = lines[i][start]
    j, k = i, start + 1
    while j < len(lines):
        s = lines[j]
        while k < len(s):
            c = s[k]
            if q == '"' and c == "\\":
                k += 2
                continue
            if c == q:
                if q == "'" and k + 1 < len(s) and s[k + 1] == "'":
                    k += 2
                    continue
                return j
            k += 1
        j, k = j + 1, 0
    return len(lines) - 1


def blank_value(lines, i, head, col, value_at):
    """Write `head ""` and skip the old value: a quoted scalar to its closing
    quote (which may sit on a later line, at any indent), then any
    continuation lines deeper than `col`."""
    out = [head + ' ""']
    v = lines[i][value_at:].lstrip()
    if v[:1] in ("'", '"'):
        i = quoted_end(lines, i, len(lines[i]) - len(v))
    i += 1
    while i < len(lines) and (lines[i].strip() == "" or indent(lines[i]) > col):
        i += 1
    return out, i


def strip_file(path: pathlib.Path, rel: str) -> int:
    text = path.read_text(encoding="utf-8")
    nl = "\r\n" if "\r\n" in text else "\n"
    lines = text.split(nl)
    out, i, n = [], 0, 0
    localized = rel.startswith("i18n/") and rel.endswith("descriptions.yaml")
    evolutions = rel.startswith("i18n/") and rel.endswith("/evolutions.yaml")
    section = ""
    while i < len(lines):
        line = lines[i]
        if line and not line[0].isspace() and not line.startswith("#"):
            section = line.split(":")[0]
        if localized:
            m = ITEM.match(line)
            if m:
                out.append(f'{m.group(1)}- ""')
                n += 1
                i += 1
                continue
        if evolutions and section == "evolution_descriptions":
            m = KEYVAL.match(line)
            if m and not m.group(3).startswith("#"):
                o, i = blank_value(lines, i, f"{m.group(1)}{m.group(2)}:", indent(line), m.start(3))
                out += o
                n += 1
                continue
        m = DESC.match(line)
        if m and m.group(2).strip() and not m.group(2).strip().startswith("#"):
            o, i = blank_value(lines, i, f"{m.group(1)}description:", len(m.group(1)), m.start(2))
            out += o
            n += 1
            continue
        out.append(line)
        i += 1
    if n:
        path.write_text(nl.join(out), encoding="utf-8", newline="")
    return n


def main() -> None:
    data = pathlib.Path(sys.argv[1]) / "data"
    total = files = 0
    for p in sorted(data.rglob("*.yaml")):
        k = strip_file(p, p.relative_to(data).as_posix())
        if k:
            files += 1
            total += k
    print(f"blanked {total} card texts in {files} files")


if __name__ == "__main__":
    main()
