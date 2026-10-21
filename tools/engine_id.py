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
import importlib
import pathlib
import sys


def definitions(root: pathlib.Path):
    """Only engine_id's reachable definitions, constants and imports.

    Imports such as rust_tokens and derived constants such as PROSE_QUOTED
    belong to the checkout's hash algorithm too. Never execute the packager's
    top-level build, file-copy or publishing statements.
    """
    text = (root / "scripts" / "build_site_app.py").read_text(encoding="utf-8")
    tree = ast.parse(text)
    bindings = {}
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            node.decorator_list = []
            bindings[node.name] = node
        elif isinstance(node, (ast.Assign, ast.AnnAssign)):
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            for target in targets:
                for name in ast.walk(target):
                    if isinstance(name, ast.Name) and isinstance(name.ctx, ast.Store):
                        bindings[name.id] = node
        elif isinstance(node, (ast.Import, ast.ImportFrom)):
            for name in node.names:
                bound = name.asname or (name.name.split('.')[0] if isinstance(node, ast.Import) else name.name)
                bindings[bound] = node
    needed, pending = set(), ['engine_id']
    while pending:
        name = pending.pop()
        if name in needed or name in {'ROOT', 'sorted'} or name not in bindings:
            continue
        needed.add(name)
        pending.extend(n.id for n in ast.walk(bindings[name]) if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load))
    if 'engine_id' not in needed:
        raise ValueError('upstream build script does not define engine_id')
    selected = {id(bindings[name]) for name in needed}
    keep = [node for node in tree.body if id(node) in selected or
            (isinstance(node, ast.ImportFrom) and node.module == '__future__')]
    return compile(ast.Module(body=keep, type_ignores=[]), "build_site_app.py", "exec")


def engine_id(root: pathlib.Path, code, fold_case: bool) -> str:
    def ordered(items, *args, **kw):
        items = list(items)
        if items and all(isinstance(p, pathlib.PurePath) for p in items) and not args and not kw:
            # Python sorts paths by their parts, folded on Windows only.
            return builtins.sorted(items, key=lambda p: [x.lower() if fold_case else x for x in p.as_posix().split("/")])
        return builtins.sorted(items, *args, **kw)
    ns = {"__name__": "wfsim_build_site_app", "__file__": str(root / 'scripts' / 'build_site_app.py'),
          "ROOT": root, "sorted": ordered}
    scripts = root / 'scripts'
    # Build scripts may examine several commits in one process. A helper from
    # another checkout must not silently determine this checkout's identity.
    local_names = {p.stem for p in scripts.glob('*.py')}
    previous = {name: sys.modules.pop(name) for name in local_names if name in sys.modules}
    old_path = sys.path[:]
    sys.path.insert(0, str(scripts))
    importlib.invalidate_caches()
    try:
        exec(code, ns)
        result = ns['engine_id']()
        if not isinstance(result, str) or len(result) != 12 or any(c not in '0123456789abcdef' for c in result):
            raise ValueError(f'invalid upstream engine id: {result!r}')
        return result
    finally:
        sys.path[:] = old_path
        for name in local_names:
            sys.modules.pop(name, None)
        sys.modules.update(previous)


if __name__ == "__main__":
    root = pathlib.Path(sys.argv[1]).resolve()
    code = definitions(root)
    for i in dict.fromkeys(engine_id(root, code, f) for f in (False, True)):
        print(i)
