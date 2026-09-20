"""留出编码任务与独立行为验收；不检查代理实现形状，不向代理暴露判题代码。"""

import subprocess
import sys


TASKS = {
    "config_layers": {
        "prompt": "Implement config.py's merge_layers and load_config and the config_cli.py CLI. merge_layers takes an iterable of JSON-object layers, recursively merges objects, replaces arrays/scalars, treats null as an ordinary replacement, preserves insertion order, and returns deeply independent data without changing inputs. Reject non-object layers with ValueError. load_config reads UTF-8 JSON files in caller order and rejects duplicate keys at any nesting level, non-finite numbers, and non-object documents; include the failing filename in ValueError diagnostics. The CLI takes one or more file paths and prints exactly one compact JSON object plus newline, with sorted keys; invalid input prints a concise error to stderr, exits 2, and leaves stdout empty. Preserve public signatures and existing tests. Add meaningful regression tests and verify the combined deliverable. Use standard libraries only. Work only in this workspace; do not delegate.",
        "files": {
            "config.py": "def merge_layers(layers):\n    raise NotImplementedError\n\ndef load_config(paths):\n    raise NotImplementedError\n",
            "config_cli.py": "def main(argv=None):\n    raise NotImplementedError\n\nif __name__ == '__main__':\n    raise SystemExit(main())\n",
            "test_existing.py": "import unittest\nfrom config import merge_layers\nclass Existing(unittest.TestCase):\n    def test_merge(self):\n        self.assertEqual(merge_layers([{'a': 1}, {'b': 2}]), {'a': 1, 'b': 2})\n",
        },
        "verify": r'''
import copy, json, subprocess, sys, tempfile
from pathlib import Path
from config import merge_layers, load_config
layers = [{'nested': {'a': [1], 'b': 2}, 'arr': [1], 'nil': 1},
          {'nested': {'c': 3}, 'arr': [2, {'x': 4}], 'nil': None}]
before = copy.deepcopy(layers)
result = merge_layers(iter(layers))
assert result == {'nested': {'a': [1], 'b': 2, 'c': 3}, 'arr': [2, {'x': 4}], 'nil': None}
assert list(result['nested']) == ['a', 'b', 'c']
result['nested']['a'].append(9); result['arr'][1]['x'] = 8
assert layers == before
assert merge_layers([]) == {}
assert merge_layers([{'x': {'a': 1}}, {'x': 0}, {'x': {'b': 2}}]) == {'x': {'b': 2}}
for bad in [None, [], 2, 'text']:
    try: merge_layers([{'ok': 1}, bad])
    except ValueError: pass
    else: raise AssertionError('non-object layer accepted')
with tempfile.TemporaryDirectory() as d:
    p, q = Path(d)/'one.json', Path(d)/'two.json'
    p.write_text(json.dumps(layers[0])); q.write_text(json.dumps(layers[1]))
    assert load_config([p, q]) == merge_layers(layers)
    run = subprocess.run([sys.executable, 'config_cli.py', str(p), str(q)], capture_output=True, text=True)
    assert run.returncode == 0 and not run.stderr
    assert run.stdout == json.dumps(merge_layers(layers), sort_keys=True, separators=(',', ':'))+'\n'
    for invalid in ['{"x":1,"x":2}', '{"a":{"x":1,"x":2}}', '{"x":NaN}', '{"x":Infinity}', '[]', '{']:
        q.write_text(invalid)
        try: load_config([p, q])
        except ValueError as exc: assert q.name in str(exc)
        else: raise AssertionError('invalid document accepted: '+invalid)
        run = subprocess.run([sys.executable, 'config_cli.py', str(p), str(q)], capture_output=True, text=True)
        assert run.returncode == 2 and not run.stdout and run.stderr.strip()
print('config acceptance passed')
''',
        "runtime": "python",
    },
    "dependency_graph": {
        "prompt": "Implement graph.mjs exports plan and affected, preserving signatures. Dependencies are a plain object mapping each node name to an array of prerequisite node names. plan returns all nodes in topological order, choosing the lexicographically smallest currently ready node at each step; repeated prerequisites count once. Validate the entire graph: reject non-array dependencies, non-string dependency entries, missing nodes and cycles with Error. affected validates the same way and returns changed nodes plus all transitive dependents, ordered as plan would order them; reject unknown changed nodes, deduplicate changed names. Support names such as __proto__ and constructor safely; do not mutate any input or use inherited properties as nodes. Add meaningful node:test regression tests and run the existing tests. No dependencies. Work only in this workspace; do not delegate.",
        "files": {
            "graph.mjs": "export function plan(dependencies) { throw new Error('not implemented'); }\nexport function affected(dependencies, changed) { throw new Error('not implemented'); }\n",
            "test_existing.mjs": "import test from 'node:test';\nimport assert from 'node:assert/strict';\nimport {plan} from './graph.mjs';\ntest('basic order', () => assert.deepEqual(plan({build:['parse'],parse:[]}), ['parse','build']));\n",
        },
        "verify": r'''
import assert from 'node:assert/strict';
import {plan, affected} from './graph.mjs';
const g = {z:[], a:['z'], c:[], b:['a','a'], d:['b','c']};
const saved = JSON.stringify(g);
assert.deepEqual(plan(g), ['c','z','a','b','d']);
assert.deepEqual(affected(g, ['a','a']), ['a','b','d']);
assert.deepEqual(affected(g, ['c','z']), ['c','z','a','b','d']);
assert.deepEqual(affected(g, []), []);
assert.equal(JSON.stringify(g), saved);
assert.deepEqual(plan({}), []);
const special = JSON.parse('{"__proto__":[],"constructor":["__proto__"],"toString":["constructor"]}');
assert.deepEqual(plan(special), ['__proto__','constructor','toString']);
assert.deepEqual(affected(special, ['__proto__']), ['__proto__','constructor','toString']);
for (const bad of [{a:'b'}, {a:[2]}, {a:['missing']}, {a:['a']}, {a:['b'],b:['a']}, {a:[],b:['c'],c:['b']}]) {
    assert.throws(() => plan(bad));
    assert.throws(() => affected(bad, []));
}
assert.throws(() => affected(g, ['missing']));
const frozen = Object.freeze({b:Object.freeze(['a']),a:Object.freeze([])});
assert.deepEqual(affected(frozen,Object.freeze(['a'])), ['a','b']);
const long = Object.fromEntries(Array.from({length:100},(_,i)=>[String(i).padStart(3,'0'),i?[String(i-1).padStart(3,'0')]:[]]));
assert.equal(plan(long).length,100);
assert.equal(affected(long,['050']).length,50);
console.log('graph acceptance passed');
''',
        "runtime": "node",
    },
    "sqlite_transfers": {
        "prompt": "Implement accounts.py AccountStore using sqlite3 and accounts_cli.py using only Python standard libraries. AccountStore(path) opens a persistent database and creates its schema if absent; close releases its connection. create(name,balance) inserts a non-empty string name with nonnegative integer balance (bool is invalid); reject invalid input and duplicates with ValueError. balance(name) returns its integer balance, unknown names raise KeyError. transfer(source,target,amount,request_id) accepts positive integer amount (not bool), distinct existing accounts, and non-empty string request_id. Atomically debit and credit or raise ValueError for insufficient funds/invalid input (KeyError for missing accounts), leaving no changes on failure. Persist idempotency: exact repeats of a successful request return False without changing balances; conflicting reuse of its id raises ValueError. A newly applied transfer returns True. Failed attempts must not consume the id. Separate instances accessing one file must see committed changes and concurrent transfers must not lose updates or overdraw. CLI: `python3 accounts_cli.py DATABASE balances NAME...` prints one compact JSON object of requested balances with sorted keys plus newline; failures use stderr and exit 2 without stdout. Preserve signatures and existing tests; add and run meaningful tests, including reopen and concurrent access. Work only in this workspace; do not delegate.",
        "files": {
            "accounts.py": "class AccountStore:\n    def __init__(self, path):\n        raise NotImplementedError\n    def close(self):\n        raise NotImplementedError\n    def create(self, name, balance):\n        raise NotImplementedError\n    def balance(self, name):\n        raise NotImplementedError\n    def transfer(self, source, target, amount, request_id):\n        raise NotImplementedError\n",
            "accounts_cli.py": "def main(argv=None):\n    raise NotImplementedError\n\nif __name__ == '__main__':\n    raise SystemExit(main())\n",
            "test_existing.py": "import unittest\nfrom accounts import AccountStore\nclass Existing(unittest.TestCase):\n    def test_empty(self):\n        store=AccountStore(':memory:')\n        try:\n            store.create('a', 4)\n            self.assertEqual(store.balance('a'), 4)\n        finally:\n            store.close()\n",
        },
        "verify": r'''
import concurrent.futures, json, subprocess, sys, tempfile
from pathlib import Path
from accounts import AccountStore
with tempfile.TemporaryDirectory() as d:
    p = Path(d)/'accounts.sqlite'
    a = AccountStore(p); a.create('source', 20); a.create('target', 0)
    for name, value in [('', 1), ('bad', True), ('bad', -1), ('bad', 1.2), ('source', 1)]:
        try: a.create(name, value)
        except ValueError: pass
        else: raise AssertionError('invalid creation accepted')
    assert a.transfer('source', 'target', 3, 'one') is True
    assert a.transfer('source', 'target', 3, 'one') is False
    try: a.transfer('source', 'target', 4, 'one')
    except ValueError: pass
    else: raise AssertionError('conflicting request accepted')
    for amount in [True, 0, -1, 1.5, 100]:
        try: a.transfer('source','target',amount,'retry')
        except ValueError: pass
        else: raise AssertionError('invalid amount accepted')
    try: a.transfer('source','missing',1,'retry')
    except KeyError: pass
    else: raise AssertionError('missing account accepted')
    assert (a.balance('source'),a.balance('target')) == (17,3)
    assert a.transfer('source','target',1,'retry') is True
    a.close()
    b = AccountStore(p)
    assert (b.balance('source'),b.balance('target')) == (16,4)
    assert b.transfer('source','target',3,'one') is False
    def move(i):
        c = AccountStore(p)
        try:
            try: return c.transfer('source','target',1,'concurrent-'+str(i))
            except ValueError: return False
        finally: c.close()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        results=list(pool.map(move,range(24)))
    assert sum(results) == 16
    assert (b.balance('source'),b.balance('target')) == (0,20)
    b.close()
    run=subprocess.run([sys.executable,'accounts_cli.py',str(p),'balances','target','source'],capture_output=True,text=True)
    assert run.returncode == 0 and not run.stderr
    assert run.stdout == '{"source":0,"target":20}\n'
    run=subprocess.run([sys.executable,'accounts_cli.py',str(p),'balances','missing'],capture_output=True,text=True)
    assert run.returncode == 2 and not run.stdout and run.stderr.strip()
print('sqlite acceptance passed')
''',
        "runtime": "python",
    },
}


# 第二轮工具适配在 SQLite 轨迹上开发；新增任务在该候选构建和运行之前冻结。
TASKS["interval_ranges"] = {
    "prompt": "Implement ranges.py normalize(ranges) and subtract(ranges,cuts), plus ranges_cli.py. Inputs are iterables of half-open integer intervals [start,end); accept two-item lists/tuples, reject bool/non-integer endpoints, reversed endpoints, malformed intervals or non-iterables with ValueError. Empty intervals are allowed but contribute nothing. normalize returns a sorted list of nonempty [start,end] lists, merging overlaps and touching intervals. subtract returns normalized ranges minus the union of cuts, handling unsorted inputs and overlaps. Preserve inputs and handle arbitrarily large integer coordinates using interval algorithms, not coordinate enumeration. CLI syntax: `python3 ranges_cli.py normalize INPUT.json` or `python3 ranges_cli.py subtract INPUT.json CUTS.json`; read UTF-8 JSON arrays of intervals and print exactly compact JSON plus newline. Invalid arguments/files/data must exit 2, print a concise stderr error and no stdout. Preserve signatures and existing tests. Add meaningful regression tests, verify the combined deliverable, use only Python standard libraries. Work only in this workspace; do not delegate.",
    "files": {
        "ranges.py": "def normalize(ranges):\n    raise NotImplementedError\n\ndef subtract(ranges, cuts):\n    raise NotImplementedError\n",
        "ranges_cli.py": "def main(argv=None):\n    raise NotImplementedError\n\nif __name__ == '__main__':\n    raise SystemExit(main())\n",
        "test_existing.py": "import unittest\nfrom ranges import normalize\nclass Existing(unittest.TestCase):\n    def test_merge(self):\n        self.assertEqual(normalize([[3,5],[1,3]]), [[1,5]])\n",
    },
    "verify": r'''
import copy, json, random, subprocess, sys, tempfile
from pathlib import Path
from ranges import normalize, subtract
assert normalize(iter([(5,7),(1,3),(3,5),(4,4)])) == [[1,7]]
assert subtract([[0,10]], [[2,4],[6,8]]) == [[0,2],[4,6],[8,10]]
big = 10**40
assert subtract([[-big,big]], [[-1,1]]) == [[-big,-1],[1,big]]
for invalid in [None, 1, 'bad', [[True,2]], [[1,False]], [[2,1]], [[0,1.0]], [[1]], [[1,2,3]], [None]]:
    for call in [lambda: normalize(invalid), lambda: subtract([[0,2]], invalid)]:
        try: call()
        except ValueError: pass
        else: raise AssertionError('invalid ranges accepted')
def points(items):
    return {p for a,b in items for p in range(a,b)}
def packed(values):
    out=[]
    for value in sorted(values):
        if out and out[-1][1] == value: out[-1][1] = value+1
        else: out.append([value,value+1])
    return out
rng=random.Random(81729)
for _ in range(100):
    a=[sorted([rng.randrange(-15,16),rng.randrange(-15,16)]) for _ in range(rng.randrange(12))]
    b=[sorted([rng.randrange(-15,16),rng.randrange(-15,16)]) for _ in range(rng.randrange(12))]
    original=copy.deepcopy((a,b))
    assert normalize(a)==packed(points(a))
    assert subtract(a,b)==packed(points(a)-points(b))
    assert (a,b)==original
with tempfile.TemporaryDirectory() as d:
    a,b=Path(d)/'a.json',Path(d)/'b.json'
    a.write_text('[[0,10]]');b.write_text('[[2,4],[6,8]]')
    run=subprocess.run([sys.executable,'ranges_cli.py','subtract',str(a),str(b)],capture_output=True,text=True)
    assert run.returncode==0 and run.stdout=='[[0,2],[4,6],[8,10]]\n' and not run.stderr
    run=subprocess.run([sys.executable,'ranges_cli.py','normalize',str(a)],capture_output=True,text=True)
    assert run.returncode==0 and run.stdout=='[[0,10]]\n' and not run.stderr
    for content in ['null','{}','[[true,2]]','[[2,1]]','not json']:
        a.write_text(content)
        run=subprocess.run([sys.executable,'ranges_cli.py','normalize',str(a)],capture_output=True,text=True)
        assert run.returncode==2 and not run.stdout and run.stderr.strip()
    run=subprocess.run([sys.executable,'ranges_cli.py'],capture_output=True,text=True)
    assert run.returncode==2 and not run.stdout and run.stderr.strip()
print('interval acceptance passed')
''',
    "runtime": "python",
}


def verify(task, workspace):
    """在独立进程执行行为判题和不可变的已有测试，避免导入缓存与代理日志冒充结果。"""
    spec = TASKS[task]
    commands = ([['node', '--input-type=module', '-e', spec['verify']],
                 ['node', '--test', 'test_existing.mjs']] if spec['runtime'] == 'node' else
                [[sys.executable, '-c', spec['verify']],
                 [sys.executable, '-m', 'unittest', 'discover', '-v']])
    for command in commands:
        try:
            result = subprocess.run(command, cwd=workspace, capture_output=True, text=True, timeout=45)
        except (OSError, subprocess.TimeoutExpired) as error:
            return False, str(error)
        if result.returncode:
            return False, (result.stdout + result.stderr)[-4000:]
    return True, 'independent acceptance passed'
