#!/usr/bin/env python3
"""Aggregate code-search operations from local Claude Code / Codex transcripts.

Prints ONLY aggregate counts and generic flag shapes; never raw commands, paths or content.
Usage: python3 -I research/transcript_search_ops.py [--selftest]
"""
import collections, glob, json, os, re, shlex, sys

C = collections.Counter
stats = collections.defaultdict(C)

SEARCH_READ = {
    'rg', 'grep', 'egrep', 'fgrep', 'ag', 'find', 'fd', 'ls', 'tree', 'cat', 'bat', 'nl', 'head', 'tail',
    'wc', 'sed', 'awk', 'jq', 'file', 'stat', 'du', 'realpath', 'readlink', 'pwd', 'which', 'less', 'sort',
    'uniq', 'cut', 'tr', 'diff', 'xxd', 'od', 'strings', 'basename', 'dirname', 'yq', 'column',
}
BUILD_TEST = {
    'cargo', 'npm', 'pnpm', 'yarn', 'npx', 'bun', 'bunx', 'make', 'pytest', 'python', 'python3', 'go', 'tsc',
    'node', 'uv', 'uvx', 'pip', 'docker', 'just', 'deno', 'vitest', 'jest', 'eslint', 'prettier', 'ruff',
    'mypy', 'rustc', 'gcc', 'cmake', 'playwright', 'pre-commit', 'biome', 'turbo', 'wrangler', 'vite',
}
WRITE_FS = {'mkdir', 'rm', 'mv', 'cp', 'touch', 'chmod', 'ln', 'tee', 'rmdir', 'apply_patch', 'perl'}
GIT_READ = {'log', 'show', 'blame', 'diff', 'grep', 'ls-files', 'ls-tree', 'status', 'rev-parse', 'branch',
            'cat-file', 'rev-list', 'shortlog', 'describe', 'tag', 'remote', 'reflog', 'merge-base', 'config',
            'name-rev', 'for-each-ref', 'show-ref', 'whatchanged'}
WRAPPERS = {'sudo', 'timeout', 'time', 'env', 'nice', 'command', 'xargs', 'exec'}
TRACK_FLAGS = {'rg', 'grep', 'find', 'fd', 'ls', 'sed', 'head', 'tail', 'wc', 'cat', 'tree', 'nl', 'awk', 'jq'}
GLOB_OK = re.compile(r'^!?(\*\*/)?\*?(\.[\w.{},]+|\*)?$|^!?(\*\*/)?\*\.\{?[\w,]+\}?$')


def norm_glob(g):
    # keep only generic extension-shaped globs; anything naming a dir/file is redacted
    if GLOB_OK.match(g):
        return g
    return ('!' if g.startswith('!') else '') + ('<dir-scoped-glob>' if '/' in g else '<named-glob>')


def norm_flag(tok):
    tok = re.sub(r'\d+', 'N', tok)
    return tok.split('=')[0] + ('=…' if '=' in tok else '')


def split_segments(cmd):
    """Split a shell line into (pipeline-position, tokens) segments."""
    cmd = re.sub(r"<<-?\s*'?(\w+)'?.*?\n\1\b", ' ', cmd, flags=re.S)  # drop heredoc bodies
    lex = shlex.shlex(cmd, posix=True, punctuation_chars='();<>|&\n')
    lex.whitespace = ' \t\r'
    lex.whitespace_split = True
    try:
        raw = list(lex)
    except ValueError:  # unbalanced quotes: fall back to naive split
        raw = re.split(r'\s+', cmd)
    segs, cur, pos = [], [], 0
    for t in raw + [';']:
        if t and set(t) <= set('();&|\n<>'):
            if cur:
                segs.append((pos, cur))
            cur = []
            pos = pos + 1 if t == '|' else 0
            if t in ('>', '>>', '<', '>&', '&>', '&>>'):
                cur = None  # next token is a redirect target, skip it
        elif cur is None:
            cur = []
        else:
            cur.append(t)
    out = []
    for pos, toks in segs:
        if toks and toks[-1] == '2':  # '2>' split as '2' then '>'
            toks = toks[:-1]
        while toks and (re.match(r'^\w+=', toks[0]) or toks[0] in WRAPPERS or toks[0] in ('{', 'do', 'then', 'else', '!', '$')):
            if toks[0] == 'timeout' and len(toks) > 1:
                toks = toks[1:]
            toks = toks[1:]
        if toks and toks[0]:
            out.append((pos, toks))
    return out


def classify(prog, toks):
    if prog == 'git':
        sub = next((t for t in toks[1:] if not t.startswith('-') and t not in ('-C',)), '')
        if '-C' in toks:  # git -C dir sub
            i = toks.index('-C'); sub = toks[i + 2] if len(toks) > i + 2 else ''
        return ('search_read' if sub in GIT_READ else 'vcs_write'), 'git ' + sub
    if prog == 'sed':
        return ('write' if any(t.startswith('-i') for t in toks) else 'search_read'), 'sed' + (' -i' if any(t.startswith('-i') for t in toks) else (' -n' if '-n' in toks else ''))
    if prog in SEARCH_READ:
        return 'search_read', prog
    if prog in BUILD_TEST:
        return 'build_test', prog
    if prog in WRITE_FS:
        return 'write', prog
    if prog in ('cd', 'echo', 'printf', 'true', 'false', 'sleep', 'export', 'source', '.', 'set', 'for', 'done',
                'if', 'fi', 'while', 'case', 'esac', 'test', '[', '[[', 'exit', 'read', 'local', 'wait', 'kill', 'ps'):
        return 'shell_misc', prog
    if prog in ('gh', 'curl', 'wget'):
        return 'network', prog
    return 'other', '<other>'


def analyze_shell(cmd, src):
    segs = split_segments(cmd)
    cats = set()
    for pos, toks in segs:
        prog = os.path.basename(toks[0])
        cat, op = classify(prog, toks)
        cats.add(cat)
        stats[f'{src}:op'][op] += 1
        stats['all:op'][op] += 1
        stats['all:cat'][cat] += 1
        if pos == 0:
            stats['all:op_pipeline_head'][op] += 1
        if cat == 'other' and os.environ.get('DEBUG_OTHER'):
            stats['dbg:other'][prog[:20]] += 1  # local inspection only, never published
        if pos > 0:
            stats['all:pipe_target'][prog if cat != 'other' else '<other>'] += 1
            if prog in ('head', 'tail'):
                n = next((t for t in toks[1:] if re.fullmatch(r'-n?\d+|\d+', t)), None)
                if n is None and '-n' in toks and toks.index('-n') + 1 < len(toks):
                    n = toks[toks.index('-n') + 1]
                stats['all:pipe_head_tail_N'][f'{prog} {bucket(n)}'] += 1
        if prog in TRACK_FLAGS:
            for t in toks[1:]:
                if t.startswith('-') and len(t) < 30:
                    stats[f'flag:{prog}'][norm_flag(t)] += 1
            stats[f'flagset:{prog}'][' '.join(sorted({norm_flag(t) for t in toks[1:] if t.startswith('-') and len(t) < 30})) or '(none)'] += 1
        if prog in ('rg', 'grep'):
            for i, t in enumerate(toks):
                if t in ('-g', '--glob', '--include', '--exclude') and i + 1 < len(toks):
                    stats[f'glob:{prog}'][norm_glob(toks[i + 1])] += 1
                elif t.startswith(('--include=', '--glob=')):
                    stats[f'glob:{prog}'][norm_glob(t.split('=', 1)[1])] += 1
                elif t in ('-t', '--type') and i + 1 < len(toks):
                    stats['rg:type'][toks[i + 1]] += 1
        if prog == 'sed' and '-n' in toks:
            for t in toks:
                m = re.fullmatch(r"(\d+),(\d+)p", t)
                if m:
                    stats['sed:range_size'][bucket(int(m[2]) - int(m[1]) + 1)] += 1
                    stats['sed:range_start'][('1' if m[1] == '1' else '>1')] += 1
        if prog == 'git':
            sub = classify(prog, toks)[1]
            if sub[4:] in ('log', 'show', 'diff', 'blame', 'grep'):
                stats[f'flag:{sub}'].update({norm_flag(t) for t in toks[2:] if t.startswith('-') and len(t) < 30})
                if sub == 'git show' and any(':' in t and not t.startswith('-') for t in toks[2:]):
                    stats['flag:git show']['<rev>:<path>'] += 1
    stats[f'{src}:cmd_cat'][' + '.join(sorted(cats)) if cats else '(empty)'] += 1
    if '2>/dev/null' in cmd or '2>&1' in cmd:
        stats['all:redirect']['2>/dev/null' if '2>/dev/null' in cmd else '2>&1'] += 1


def bucket(n):
    try:
        n = int(str(n).lstrip('-n'))
    except (TypeError, ValueError):
        return 'default'
    for b in (10, 20, 40, 60, 80, 100, 120, 150, 200, 300, 400, 500, 1000):
        if n <= b:
            return f'<={b}'
    return '>1000'


def claude_tool(name, inp):
    stats['claude:tool'][name] += 1
    if name == 'Bash':
        analyze_shell(inp.get('command', ''), 'claude')
    elif name == 'Grep':
        stats['Grep:params'][' '.join(sorted(k for k in inp if k != 'pattern'))] += 1
        stats['Grep:output_mode'][inp.get('output_mode', '(default files_with_matches)')] += 1
        for k in ('-n', '-i', '-A', '-B', '-C', 'context', 'multiline', 'head_limit', 'glob', 'type', 'path', 'offset'):
            if k in inp:
                stats['Grep:param_used'][k] += 1
        if 'head_limit' in inp:
            stats['Grep:head_limit'][bucket(inp['head_limit'])] += 1
        if 'glob' in inp:
            stats['Grep:glob'][norm_glob(str(inp['glob']))] += 1
        if 'type' in inp:
            stats['Grep:type'][str(inp['type'])] += 1
        p = inp.get('pattern', '')
        stats['Grep:pattern_shape']['regex alternation |' if '|' in p else ('regex meta' if re.search(r'[\\.*+?\[\]()^$]', p) else 'literal')] += 1
    elif name == 'Glob':
        p = inp.get('pattern', '')
        stats['Glob:shape']['**/ recursive' if '**' in p else 'single-level'] += 1
        stats['Glob:path_given']['yes' if inp.get('path') else 'no'] += 1
    elif name == 'Read':
        has_o, has_l = 'offset' in inp, 'limit' in inp
        stats['Read:range'][{(0, 0): 'whole file', (1, 1): 'offset+limit', (0, 1): 'limit only', (1, 0): 'offset only'}[(has_o, has_l)]] += 1
        if has_l:
            stats['Read:limit'][bucket(inp['limit'])] += 1
        ext = os.path.splitext(inp.get('file_path', ''))[1].lower()
        stats['Read:ext'][ext if re.fullmatch(r'\.\w{1,6}', ext) else '(none/other)'] += 1


EXEC_CMD = re.compile(r'exec_command\(\s*\{\s*cmd\s*:\s*("(?:[^"\\]|\\.)*"|\'(?:[^\'\\]|\\.)*\'|`(?:[^`\\]|\\.)*`)(.*?)\}\s*\)', re.S)


def js_str(lit):
    if lit[0] == '"':
        try:
            return json.loads(lit)
        except ValueError:
            pass
    return re.sub(r'\\(.)', lambda m: {'n': '\n', 't': '\t'}.get(m[1], m[1]), lit[1:-1])


def codex_cmd(cmd, extra_keys):
    stats['codex:tool']['exec_command'] += 1
    analyze_shell(cmd, 'codex')
    for k in extra_keys:
        stats['codex:exec_param'][k] += 1


def run():
    home = os.path.expanduser('~')
    files = glob.glob(f'{home}/.claude/projects/**/*.jsonl', recursive=True)
    stats['meta:files']['claude'] = len(files)
    for f in files:
        for line in open(f, errors='replace'):
            try:
                d = json.loads(line)
            except ValueError:
                continue
            m = d.get('message')
            if isinstance(m, dict) and isinstance(m.get('content'), list):
                for b in m['content']:
                    if isinstance(b, dict) and b.get('type') == 'tool_use' and isinstance(b.get('input'), dict):
                        claude_tool(b['name'], b['input'])
    files = glob.glob(f'{home}/.codex/**/*.jsonl', recursive=True)
    stats['meta:files']['codex'] = len(files)
    for f in files:
        for line in open(f, errors='replace'):
            try:
                d = json.loads(line)
            except ValueError:
                continue
            p = d.get('payload') if isinstance(d.get('payload'), dict) else {}
            if d.get('type') != 'response_item':
                continue
            if p.get('type') == 'custom_tool_call' and p.get('name') == 'exec':
                for lit, rest in EXEC_CMD.findall(p.get('input', '')):
                    codex_cmd(js_str(lit), re.findall(r'(\w+)\s*:', rest))
                    m = re.search(r'max_output_tokens\s*:\s*(\d+)', rest)
                    if m:
                        stats['codex:max_output_tokens'][m[1]] += 1
                for t in re.findall(r'tools\.(\w+)\(', p.get('input', '')):
                    if t != 'exec_command':
                        stats['codex:tool'][t if not t.startswith('mcp__') else 'mcp__*'] += 1
            elif p.get('type') == 'function_call' and p.get('name') == 'exec_command':
                try:
                    a = json.loads(p.get('arguments', '{}'))
                except ValueError:
                    continue
                cmd = a.get('cmd', '')
                codex_cmd(' '.join(cmd) if isinstance(cmd, list) else cmd, [k for k in a if k != 'cmd'])
                if 'max_output_tokens' in a:
                    stats['codex:max_output_tokens'][str(a['max_output_tokens'])] += 1
            elif p.get('type') in ('function_call', 'custom_tool_call'):
                stats['codex:tool'][p.get('name')] += 1


def report(top=25):
    for key in sorted(stats):
        c = stats[key]
        tot = sum(c.values())
        print(f'\n## {key}  (total {tot})')
        for k, v in c.most_common(top):
            if v >= 3 or key.startswith('meta'):  # drop rare long-tail values (also limits leakage)
                print(f'{v:>8}  {v / tot:6.1%}  {k}')


def selftest():
    segs = split_segments("cd x && rg -n -i 'foo' -g '*.rs' src | head -50; FOO=1 timeout 5 sed -n '10,80p' a.rs")
    assert [s[1][0] for s in segs] == ['cd', 'rg', 'head', 'sed'], segs
    assert segs[2][0] == 1
    segs = split_segments("grep -nE 'fn|pub;x' a 2>/dev/null | head -5\nfor f in a; do cat $f; done 2>&1")
    assert [(p, s[0]) for p, s in segs] == [(0, 'grep'), (1, 'head'), (0, 'for'), (0, 'cat'), (0, 'done')], segs
    assert classify('git', ['git', '-C', 'd', 'log', '--oneline'])[1] == 'git log'
    assert norm_glob('*.rs') == '*.rs' and norm_glob('src/private/**') == '<dir-scoped-glob>' and norm_glob('!vendor') == '!<named-glob>'
    assert bucket(50) == '<=60' and bucket('-n20') == '<=20'
    assert js_str('"a\\nb"') == 'a\nb'
    m = EXEC_CMD.findall('tools.exec_command({cmd:"sed -n \'1,20p\' f", yield_time_ms:1000})')
    assert js_str(m[0][0]).startswith('sed') and 'yield_time_ms' in m[0][1]
    print('selftest ok')


if __name__ == '__main__':
    selftest() if '--selftest' in sys.argv else (run(), report())
