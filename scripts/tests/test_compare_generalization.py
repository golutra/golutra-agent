"""判题器必须识别缺失实现；任务摘要和成功判定不得被运行时的成功消息替代。"""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_generalization as suite
import generalization_tasks as tasks


class GeneralizationTests(unittest.TestCase):
    def test_recovery_count_uses_runtime_retries_not_successful_adapter_attempts(self):
        import json
        events = []
        for index, phase in enumerate(["waiting", "retrying"]):
            event = {"event_type": "retry_scheduled", "id": str(index),
                     "payload": {"recovery": {"phase": phase, "network": False,
                                               "error_metadata": {"http_status": 504}}}}
            events.extend([{"type": "runtime.event", "event": event},
                           {"type": "item.completed", "item": {"data": event}}])
        result = suite.recovery_summary("\n".join(map(json.dumps, events)))
        self.assertEqual(result, {"retry_attempts": 1, "network_retry_attempts": 0, "http_statuses": [504]})

    def test_seeded_implementations_fail_independent_acceptance(self):
        for name, spec in tasks.TASKS.items():
            with self.subTest(task=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for path, content in spec["files"].items():
                    (root / path).write_text(content)
                passed, detail = tasks.verify(name, root)
                self.assertFalse(passed)
                self.assertTrue(detail)

    def test_judges_parse_before_any_paid_run(self):
        for name, spec in tasks.TASKS.items():
            with self.subTest(task=name):
                if spec["runtime"] == "python":
                    compile(spec["verify"], name, "exec")
                else:
                    result = subprocess.run(
                        ["node", "--input-type=module", "--check"], input=spec["verify"],
                        text=True, capture_output=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)

    def test_task_digest_includes_hidden_acceptance_and_visible_fixture(self):
        spec = suite.task_spec("config_layers")
        original = suite.task_digest(spec)
        self.assertNotEqual(original, suite.task_digest({**spec, "verify": spec["verify"] + "\n# changed"}))
        self.assertNotEqual(original, suite.task_digest({**spec, "files": {}}))
        self.assertNotEqual(original, suite.task_digest({**spec, "prompt": "different"}))

    def test_runtime_success_does_not_override_failed_or_tampered_delivery(self):
        metrics = {"completed": True, "runtime_terminal_success": True, "return_code": 0}
        self.assertFalse(suite.result_passed(metrics, False, True))
        self.assertFalse(suite.result_passed(metrics, True, False))
        self.assertTrue(suite.result_passed(metrics, True, True))

    def test_graph_judge_accepts_an_independent_reference(self):
        source = """
export function plan(g) {
  const indegree=new Map(), edges=new Map();
  for (const k of Object.keys(g)) {indegree.set(k,0);edges.set(k,[]);}
  for (const k of Object.keys(g)) {
    if (!Array.isArray(g[k])) throw Error('invalid');
    for (const d of new Set(g[k])) {
      if(typeof d!=='string'||!indegree.has(d))throw Error('invalid');
      indegree.set(k,indegree.get(k)+1);edges.get(d).push(k);
    }
  }
  const ready=[...indegree].filter(([,n])=>!n).map(([k])=>k), out=[];
  while(ready.length) {
    ready.sort();const k=ready.shift();out.push(k);
    for(const d of edges.get(k)) {
      indegree.set(d,indegree.get(d)-1);if(!indegree.get(d))ready.push(d);
    }
  }
  if(out.length!==indegree.size)throw Error('cycle');return out;
}
export function affected(g,changed) {
  const order=plan(g), chosen=new Set(changed);
  for(const k of chosen)if(!Object.hasOwn(g,k))throw Error('unknown');
  for(const k of order)if(g[k].some(d=>chosen.has(d)))chosen.add(k);
  return order.filter(k=>chosen.has(k));
}
"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path, content in tasks.TASKS["dependency_graph"]["files"].items():
                (root / path).write_text(content)
            (root / "graph.mjs").write_text(source)
            passed, detail = tasks.verify("dependency_graph", root)
            self.assertTrue(passed, detail)

    def test_interval_judge_accepts_an_independent_reference(self):
        source = '''
def normalize(ranges):
    try: items=list(ranges)
    except TypeError as exc: raise ValueError('iterable required') from exc
    for pair in items:
        if not isinstance(pair,(list,tuple)) or len(pair)!=2 or any(type(v)!=int for v in pair) or pair[0]>pair[1]:
            raise ValueError('invalid interval')
    result=[]
    for a,b in sorted(items):
        if a==b: continue
        if result and a<=result[-1][1]: result[-1][1]=max(b,result[-1][1])
        else: result.append([a,b])
    return result
def subtract(ranges,cuts):
    result=normalize(ranges)
    for low,high in normalize(cuts):
        remaining=[]
        for a,b in result:
            if b<=low or a>=high: remaining.append([a,b])
            else:
                if a<low: remaining.append([a,low])
                if high<b: remaining.append([high,b])
        result=remaining
    return result
'''
        cli = '''
import json,sys
from ranges import normalize,subtract
def main(argv=None):
    args=sys.argv[1:] if argv is None else argv
    try:
        if not args or args[0] not in ('normalize','subtract') or len(args)!=(2 if args[0]=='normalize' else 3): raise ValueError('usage')
        values=[]
        for path in args[1:]:
            with open(path,encoding='utf-8') as f: value=json.load(f)
            if not isinstance(value,list): raise ValueError('array required')
            values.append(value)
        result=(normalize if args[0]=='normalize' else subtract)(*values)
    except (ValueError,OSError) as exc:
        print(str(exc),file=sys.stderr);return 2
    print(json.dumps(result,separators=(',',':')));return 0
if __name__=='__main__': raise SystemExit(main())
'''
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path, content in tasks.TASKS["interval_ranges"]["files"].items():
                (root / path).write_text(content)
            (root / "ranges.py").write_text(source)
            (root / "ranges_cli.py").write_text(cli)
            passed, detail = tasks.verify("interval_ranges", root)
            self.assertTrue(passed, detail)

    def test_config_judge_accepts_an_independent_reference(self):
        source = """
import copy, json
def merge_layers(layers):
    def merge(a,b):
        out=copy.deepcopy(a)
        for k,v in b.items():
            out[k]=merge(out[k],v) if isinstance(out.get(k),dict) and isinstance(v,dict) else copy.deepcopy(v)
        return out
    out={}
    for layer in layers:
        if not isinstance(layer,dict): raise ValueError('object required')
        out=merge(out,layer)
    return out
def load_config(paths):
    def pairs(items):
        result={}
        for k,v in items:
            if k in result: raise ValueError('duplicate key')
            result[k]=v
        return result
    def invalid(value): raise ValueError(value)
    layers=[]
    for path in paths:
        try:
            with open(path,encoding='utf-8') as f:
                value=json.load(f,object_pairs_hook=pairs,parse_constant=invalid)
            if not isinstance(value,dict): raise ValueError('object required')
            layers.append(value)
        except (OSError,ValueError) as exc: raise ValueError(str(path)+': '+str(exc)) from exc
    return merge_layers(layers)
"""
        cli = """
import sys,json
from config import load_config
def main(argv=None):
    try:
        value=load_config(sys.argv[1:] if argv is None else argv)
    except ValueError as exc:
        print(str(exc),file=sys.stderr); return 2
    print(json.dumps(value,sort_keys=True,separators=(',',':'))); return 0
if __name__=='__main__': raise SystemExit(main())
"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path, content in tasks.TASKS["config_layers"]["files"].items():
                (root / path).write_text(content)
            (root / "config.py").write_text(source)
            (root / "config_cli.py").write_text(cli)
            passed, detail = tasks.verify("config_layers", root)
            self.assertTrue(passed, detail)


if __name__ == "__main__":
    unittest.main()
