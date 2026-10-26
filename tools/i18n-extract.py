"""Adds the strings the code puts through tr(, trf(, n_(, trc(ctx, ...) and Slint's @tr(...) /
@tr(ctx => ...) that the English or Arabic .po lacks, with an empty msgstr to fill in (`cargo t`
fails until every one has its translation).
Usage: python tools/i18n-extract.py [--list]"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
POS = [os.path.join(ROOT, 'i18n', code, 'LC_MESSAGES', 'nowbar-ui.po') for code in ('en', 'ar')]
LIT = r'"((?:[^"\\]|\\.)*)"'
# trc("ctx", "text"), @tr("ctx" => "text"), then the one-string calls.
WITH_CONTEXT = re.compile(r'(?<![\w.])(?:trc\(' + LIT + r'\s*,\s*' + LIT + r'|@?tr\(' + LIT + r'\s*=>\s*' + LIT + r')')
PLAIN = re.compile(r'(?<![\w.])(?:@?tr|trf|n_)\(' + LIT + r'(?!\s*=>)')


def unescape(s):
    s = re.sub(r'\\u\{([0-9a-fA-F]+)\}', lambda m: chr(int(m.group(1), 16)), s)
    return re.sub(r'\\(.)', lambda m: {'n': '\n', 't': '\t'}.get(m.group(1), m.group(1)), s)


def po_escape(s):
    return s.replace('\\', '\\\\').replace('"', '\\"').replace('\n', '\\n').replace('\t', '\\t')


def known(po):
    """The (context, msgid) pairs a .po has, translated or not."""
    text = open(po, encoding='utf-8').read()
    pairs = set()
    for block in re.split(r'\n\s*\n', text):
        ctx = re.search(r'^msgctxt ' + LIT, block, re.M)
        m = re.search(r'^msgid ' + LIT + r'((?:\n' + LIT + r')*)', block, re.M)
        if m:
            whole = m.group(1) + ''.join(re.findall(LIT, m.group(2)))
            pairs.add((unescape(ctx.group(1)) if ctx else None, unescape(whole)))
    return pairs


def used():
    """Every wrapped (context, string), with the first file it's in."""
    found = {}
    for base in ('src', 'ui', os.path.join('setup', 'src'), os.path.join('setup', 'ui')):
        for dirpath, _, files in os.walk(os.path.join(ROOT, base)):
            for f in sorted(files):
                if not f.endswith(('.rs', '.slint')) or f in ('i18n.rs', 'po.rs'):
                    continue
                path = os.path.join(dirpath, f)
                text = open(path, encoding='utf-8').read()
                where = os.path.relpath(path, ROOT).replace(os.sep, '/')
                for m in WITH_CONTEXT.finditer(text):
                    ctx, s = (m.group(1), m.group(2)) if m.group(1) is not None else (m.group(3), m.group(4))
                    found.setdefault((unescape(ctx), unescape(s)), where)
                for m in PLAIN.finditer(text):
                    found.setdefault((None, unescape(m.group(1))), where)
    return found


strings = used()
for po in POS:
    have = known(po)
    missing = {k: where for k, where in strings.items() if k not in have}
    if '--list' in sys.argv:
        for (ctx, s), where in missing.items():
            print(f'{os.path.relpath(po, ROOT)}: {where}: {ctx + " => " if ctx else ""}{s!r}')
        continue
    with open(po, 'a', encoding='utf-8', newline='\n') as out:
        for (ctx, s), where in missing.items():
            context = f'msgctxt "{po_escape(ctx)}"\n' if ctx else ''
            out.write(f'\n#: {where}\n{context}msgid "{po_escape(s)}"\nmsgstr ""\n')
    print(f'{len(missing)} added to {os.path.relpath(po, ROOT)}')
