"""keripy tests written in KERI 1.0 alone, replayed in KERI 2.00 native.

A replay is the keripy test file at the pin with its protocol version and body
kind renamed to KERI 2.00 and native CESR, and its value checks made unable to
fail: those compare against KERI 1.0 bytes. The test then runs its scenario to
the end, and what keripy answers at each step is what the corpus records.

The tests assemble some messages by hand, counting a group in items as CESR 1.0
does; CESR 2.00 counts it in quadlets, so those counts are recomputed.
"""
import ast
import os
import shutil

REPLAYS = {
    "tests/core/test_weighted_threshold.py": ["test_weighted"],
    "tests/core/test_delegating.py": ["test_delegation"],
    "tests/core/test_witness.py": [
        "test_indexed_witness_replay_v1",
        "test_nonindexed_witness_receipts_v1",
        "test_out_of_order_witnessed_events",
    ],
}
ROOT = "/tmp/replay"


# A seal source couple: a sequence number under `0A` (24 characters) and a SAID (44).
_SEAL_SOURCE_COUPLE_QUADLETS = (24 + 44) // 4


class _Replay(ast.NodeTransformer):
    """An assert, or a `pytest.raises` block, becomes a statement that cannot fail the test,
    and a counter built by hand counts quadlets."""

    def visit_Call(self, node):
        self.generic_visit(node)
        code = node.args[0].attr if node.args and isinstance(node.args[0], ast.Attribute) else None
        for keyword in node.keywords:
            if keyword.arg != "count":
                continue
            if code == "ControllerIdxSigs" and _is_len_of_name(keyword.value):
                items = keyword.value.args[0].id
                keyword.value = ast.parse(
                    f"sum(len(_item.qb64b) for _item in {items}) // 4", mode="eval").body
            elif code == "SealSourceCouples" and ast.unparse(keyword.value) == "1":
                keyword.value = ast.Constant(_SEAL_SOURCE_COUPLE_QUADLETS)
        return node

    def visit_Assert(self, node):
        return _swallow([ast.Expr(node.test)], node)

    def visit_With(self, node):
        self.generic_visit(node)
        if any(_is_raises(item.context_expr) for item in node.items):
            return _swallow(node.body, node)
        return node


def _is_len_of_name(expr):
    return (isinstance(expr, ast.Call) and isinstance(expr.func, ast.Name)
            and expr.func.id == "len" and len(expr.args) == 1
            and isinstance(expr.args[0], ast.Name))


def _is_raises(expr):
    return (isinstance(expr, ast.Call) and isinstance(expr.func, ast.Attribute)
            and expr.func.attr == "raises")


def _swallow(body, at):
    handler = ast.ExceptHandler(type=ast.Name("Exception", ast.Load()), name=None,
                                body=[ast.Pass()])
    return ast.copy_location(ast.Try(body=body, handlers=[handler], orelse=[], finalbody=[]), at)


def _replay(source, keep):
    source = source.replace("Vrsn_1_0", "Vrsn_2_0").replace("Kinds.json", "Kinds.cesr")
    tree = ast.parse(source)
    tree.body = [n for n in tree.body if not (isinstance(n, ast.FunctionDef)
                                              and n.name.startswith("test_")
                                              and n.name not in keep)]
    return ast.unparse(ast.fix_missing_locations(_Replay().visit(tree)))


def prepare():
    """Writes the replays under ROOT, beside keripy's fixtures; returns their paths."""
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(f"{ROOT}/tests/core")
    shutil.copy("/opt/keripy/tests/conftest.py", f"{ROOT}/tests/conftest.py")
    shutil.copy("/opt/keripy/tests/core/__init__.py", f"{ROOT}/tests/core/__init__.py")
    for path, keep in REPLAYS.items():
        with open(f"/opt/keripy/{path}", encoding="utf-8") as f:
            replay = _replay(f.read(), set(keep))
        with open(f"{ROOT}/{path}", "w", encoding="utf-8") as f:
            f.write(replay)
    return list(REPLAYS)
