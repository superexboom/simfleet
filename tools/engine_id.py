# SPDX-License-Identifier: AGPL-3.0-or-later
"""WFSim's engine id (scripts/build_site_app.py `engine_id`) for a checkout.

THE CHECKOUT'S OWN CODE computes it: `engine_id` and what it reads
(`ENGINE_SOURCES`, `engine_files`, `lean_yaml`, ...) are taken out of that
commit's build_site_app.py and run here, so a commit that changes how the id is
counted is counted as that commit counts it. (WFSim moved to reading the data
lean, comments and blank lines gone, and tests left out, in 6c6bd917 - "the
official data pack"; a copy of the old rule here kept computing the old id.)
Only the definitions are run, never the script.

Prints every spelling it can take: paths sort case-insensitively on Windows
and case-sensitively elsewhere, so both orders are printed. The case-sensitive
one first: WFSim's site is built on its CI's Linux runner since 532015dd
("ship: a push of source is a release"), so that is the id wfsim.app serves.

    python tools/engine_id.py <wfsim checkout>
"""
import ast
import builtins
import hashlib
import json
import os
import pathlib
import re
import sys

NAMES = ("ENGINE_SOURCES", "ENGINE_SOURCES_NOT")


def definitions(root: pathlib.Path):
    """The checkout's top-level constants the id reads and every function its
    script defines, compiled - defined only, nothing called."""
    text = (root / "scripts" / "build_site_app.py").read_text(encoding="utf-8")
    tree = ast.parse(text)
    keep = []
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            node.decorator_list = []
            keep.append(node)
        elif isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in NAMES for t in node.targets):
            keep.append(node)
    return compile(ast.Module(body=keep, type_ignores=[]), "build_site_app.py", "exec")


def engine_id(root: pathlib.Path, code, fold_case: bool) -> str:
    def ordered(items, *args, **kw):
        items = list(items)
        if items and all(isinstance(p, pathlib.PurePath) for p in items) and not args and not kw:
            # Python sorts paths by their parts, folded on Windows only.
            return builtins.sorted(items, key=lambda p: [x.lower() if fold_case else x for x in p.as_posix().split("/")])
        return builtins.sorted(items, *args, **kw)
    ns = {"__name__": "wfsim_build_site_app", "ROOT": root, "Path": pathlib.Path, "hashlib": hashlib,
          "json": json, "os": os, "re": re, "sys": sys, "sorted": ordered}
    exec(code, ns)
    return ns["engine_id"]()


if __name__ == "__main__":
    root = pathlib.Path(sys.argv[1]).resolve()
    code = definitions(root)
    for i in dict.fromkeys(engine_id(root, code, f) for f in (False, True)):
        print(i)
