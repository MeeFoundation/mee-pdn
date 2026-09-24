#!/usr/bin/env -S python3 -I
"""Derives the root's OpenSpec skills (.claude, .agents) from the ones `openspec update` writes in mia-docs.

The workspace root is not an openspec root — the specs live in the sibling repo `mia-docs/` — so the
skills here are mia-docs' generated ones, with every `openspec` command told to run from `mia-docs/`.
Nothing else differs: project specifics live in mia-docs/openspec/config.yaml, which the generated
skills read at run time. Codex's UI metadata (`agents/openai.yaml`) is kept by hand and left alone; a
skill that vanished upstream is removed here too.
"""

import sys

# Before any other import: without -I a module file beside the script shadows the standard library.
if not sys.flags.isolated:
    sys.exit(f'{sys.argv[0]}: run in isolated mode, python3 -I')

import os
import re
import shutil
from pathlib import Path

CMD = re.compile(r'(^|[^A-Za-z0-9_./-])openspec (status|instructions|store|new|list|archive|schemas|validate|context|show|doctor|view)(?![A-Za-z0-9-])')
NOTE = 'The OpenSpec root is the sibling repo `mia-docs/`; every `openspec` command below runs from there.\n'


def derive(text):
    head, sep, body = text.partition('\n---\n')
    if not sep:
        raise SystemExit('no frontmatter')
    head = head.replace('allowed-tools: Bash(openspec:*)', 'allowed-tools: Bash(cd mia-docs && openspec:*)')
    text = head + sep + '\n' + NOTE + body
    text, n = CMD.subn(lambda m: m.group(1) + 'cd mia-docs && openspec ' + m.group(2), text)
    return text, n


def main():
    os.chdir(Path(__file__).resolve().parent.parent)
    for src_root, dst_root in (('mia-docs/.claude/skills', '.claude/skills'), ('mia-docs/.agents/skills', '.agents/skills')):
        src_root, dst_root = Path(src_root), Path(dst_root)
        names = sorted(d.name for d in src_root.glob('openspec-*') if (d / 'SKILL.md').is_file())
        if not names:
            raise SystemExit('no openspec-* skills under ' + str(src_root) + ' — run openspec update in mia-docs first')
        for name in names:
            text, n = derive((src_root / name / 'SKILL.md').read_text())
            if n == 0:
                raise SystemExit(name + ': no openspec command found to prefix — the upstream text changed shape, adjust this script')
            dst = dst_root / name / 'SKILL.md'
            dst.parent.mkdir(parents=True, exist_ok=True)
            dst.write_text(text)
            print('derived ' + str(dst) + ' (' + str(n) + ' commands prefixed)')
        for stale in sorted(d for d in dst_root.glob('openspec-*') if d.name not in names):
            shutil.rmtree(stale)
            print('removed ' + str(stale) + ' (no longer generated upstream)')


if __name__ == '__main__':
    main()
