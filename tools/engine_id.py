# SPDX-License-Identifier: AGPL-3.0-or-later
"""WFSim's engine id (scripts/build_site_app.py `engine_id`) for a checkout.

Prints every spelling it can take: the paths are sorted by their parts, which
Python does case-insensitively on Windows and case-sensitively elsewhere, so a
tree is matched against the served id under both orders (as `simfleet
engine-id` does).

    python tools/engine_id.py <wfsim checkout>
"""
import hashlib
import pathlib
import sys

import re

# As of WFSim 2fb8138: used when the checkout's build_site_app.py cannot be read.
SOURCES = ("engine/src", "webapi/src", "wasm/src", "tools/gas-macro", "data")
NOT = ("data/i18n/", "data/board_state.yaml", "data/market.yaml",
       "data/assets.yaml", "data/notes.yaml", "data/README.md")


def lists(root: pathlib.Path):
    """THE CHECKOUT'S OWN LISTS (ENGINE_SOURCES, ENGINE_SOURCES_NOT), so a
    commit that changes what the id covers is hashed as that commit does."""
    try:
        text = (root / "scripts" / "build_site_app.py").read_text(encoding="utf-8")
    except OSError:
        return SOURCES, NOT
    def tup(name, default):
        m = re.search(r"^" + name + r"\s*=\s*\((.*?)\)", text, re.S | re.M)
        got = tuple(re.findall(r'"([^"]*)"', m.group(1))) if m else ()
        return got or default
    return tup("ENGINE_SOURCES", SOURCES), tup("ENGINE_SOURCES_NOT", NOT)


def engine_id(root: pathlib.Path, fold_case: bool) -> str:
    sources, skip = lists(root)
    rels = sorted((p.relative_to(root).as_posix() for d in sources for p in (root / d).rglob("*") if p.is_file()),
                  key=lambda r: [x.lower() if fold_case else x for x in r.split("/")])
    rels.append("Cargo.lock")
    h = hashlib.sha256()
    for rel in rels:
        if rel.startswith(skip):
            continue
        h.update(rel.encode("utf-8") + bytes(1))
        h.update((root / rel).read_bytes().replace(b"\r\n", b"\n") + bytes(1))
    return h.hexdigest()[:12]


if __name__ == "__main__":
    root = pathlib.Path(sys.argv[1])
    for i in dict.fromkeys(engine_id(root, f) for f in (True, False)):
        print(i)
