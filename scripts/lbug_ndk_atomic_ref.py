#!/usr/bin/env -S python3 -I
"""Lets LadybugDB's C API build against the Android NDK's C++ library, which lacks std::atomic_ref.

The C API takes a database's and a connection's handle with std::atomic_ref; where the standard
library has no atomic_ref, the compiler's own atomic exchange does the same. Run on the source tree
scripts/lbug-vector-source.sh assembles, given as the only argument; a file already patched is
left as it is.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

from pathlib import Path

HANDLES = [
    ('database', 'atomic_db', 'database->_database'),
    ('connection', 'atomic_conn', 'connection->_connection'),
]


def main() -> None:
    root = Path(sys.argv[1])
    for name, var, field in HANDLES:
        path = root / 'src' / 'c_api' / f'{name}.cpp'
        text = path.read_text()
        old = f'    std::atomic_ref<void*> {var}({field});\n    void* ptr = {var}.exchange(nullptr);\n'
        if old not in text:
            continue
        new = (
            f'#if defined(__cpp_lib_atomic_ref)\n{old}#else\n'
            f'    void* ptr = __atomic_exchange_n(&{field}, nullptr, __ATOMIC_SEQ_CST);\n#endif\n'
        )
        path.write_text(text.replace(old, new, 1))


if __name__ == '__main__':
    main()
