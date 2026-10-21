# SPDX-License-Identifier: AGPL-3.0-or-later
"""Hash extraction must follow upstream helpers without running its build."""
import hashlib
import pathlib
import sys
import tempfile
import unittest

import engine_id


class EngineIdTests(unittest.TestCase):
    def test_dependencies_are_loaded_from_each_checkout_without_build_side_effects(self):
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory)
            old_path = sys.path[:]
            for value in ['first', 'other']:
                root = base / value
                (root / 'scripts').mkdir(parents=True)
                (root / 'scripts/rust_tokens.py').write_text(f'def digest_text(value): return value + {value!r}\n', encoding='utf-8')
                (root / 'scripts/build_site_app.py').write_text('''
import hashlib
import re
import rust_tokens
import definitely_not_a_build_dependency
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent
ENGINE_ID_NOT = ("excluded",)
PROSE_QUOTED = re.compile("text")
UNRELATED = (ROOT / "side-effect").write_text("bad")
def helper():
    return rust_tokens.digest_text(PROSE_QUOTED.sub(ENGINE_ID_NOT[0], "text"))
def engine_id():
    return hashlib.sha256(helper().encode()).hexdigest()[:12]
raise RuntimeError("the full build script must never execute")
''', encoding='utf-8')
                code = engine_id.definitions(root)
                self.assertEqual(engine_id.engine_id(root, code, False), hashlib.sha256(('excluded' + value).encode()).hexdigest()[:12])
                self.assertFalse((root / 'side-effect').exists())
            self.assertEqual(sys.path, old_path)

    def test_path_order_remains_explicit_and_old_hash_definitions_work(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / 'scripts').mkdir()
            (root / 'scripts/build_site_app.py').write_text('''
import hashlib
from pathlib import Path
ENGINE_SOURCES = ("a", "Z")
def engine_id():
    paths = sorted(ROOT / p for p in ENGINE_SOURCES)
    return hashlib.sha256("/".join(p.name for p in paths).encode()).hexdigest()[:12]
''', encoding='utf-8')
            code = engine_id.definitions(root)
            for folded, value in [(False, 'Z/a'), (True, 'a/Z')]:
                self.assertEqual(engine_id.engine_id(root, code, folded), hashlib.sha256(value.encode()).hexdigest()[:12])


if __name__ == '__main__':
    unittest.main()
