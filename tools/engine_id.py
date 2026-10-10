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

SOURCES = ("engine/src", "webapi/src", "wasm/src", "data")
NOT = ("data/i18n/", "data/board_state.yaml", "data/market.yaml",
       "data/assets.yaml", "data/notes.yaml", "data/README.md")


def engine_id(root: pathlib.Path, fold_case: bool) -> str:
    rels = sorted((p.relative_to(root).as_posix() for d in SOURCES for p in (root / d).rglob("*") if p.is_file()),
                  key=lambda r: [x.lower() if fold_case else x for x in r.split("/")])
    rels.append("Cargo.lock")
    h = hashlib.sha256()
    for rel in rels:
        if rel.startswith(NOT):
            continue
        h.update(rel.encode("utf-8") + bytes(1))
        h.update((root / rel).read_bytes().replace(b"\r\n", b"\n") + bytes(1))
    return h.hexdigest()[:12]


if __name__ == "__main__":
    root = pathlib.Path(sys.argv[1])
    for i in dict.fromkeys(engine_id(root, f) for f in (True, False)):
        print(i)
