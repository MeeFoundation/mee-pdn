#!/usr/bin/env -S python3 -I
"""Prints the one test executable whose target name contains the argument, from cargo's JSON messages on stdin."""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import json


def main():
    want = sys.argv[1]
    hits = []
    for line in sys.stdin:
        try:
            m = json.loads(line)
        except ValueError:
            continue
        t = m.get('target', {})
        if (m.get('reason') == 'compiler-artifact' and m.get('executable')
                and t.get('kind') == ['test'] and want in t.get('name', '')):
            hits.append((t['name'], m['executable']))
    if len(hits) != 1:
        names = ', '.join(n for n, _ in hits) or '(none)'
        sys.exit(f'want exactly one test binary matching "{want}"; matched: {names}')
    print(hits[0][1])


if __name__ == '__main__':
    main()
