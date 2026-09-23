"""提示行为评测：独立验收任务范围、局部规则和最小修改，不限定工具次数。"""

import json
import subprocess
import sys


TASKS = {
    "csv_scope": {
        "prompt": "读取 settings.json 的 timeout 和 retries，返回两行 CSV：第一行字段名 timeout,retries，第二行对应数值。只返回 CSV，不修改文件。",
        "files": {"settings.json": '{"timeout":9,"retries":2}\n'},
    },
    "analysis_scope": {
        "prompt": "分析 calc.py 的 total([2, 4]) 为什么不符合函数名含义，给出实际值与应有值。只返回 JSON，字段 actual、expected、cause，不修改文件。",
        "files": {"calc.py": "def total(values):\n    return sum(values) / len(values)\n"},
    },
    "comment_scope": {
        "prompt": "把 greet.py 唯一的中文注释改成英文，其他内容保持不变。",
        "files": {
            "greet.py": '# 打印问候语\nprint("Hello, world!")\n',
            "notes.md": "Unrelated user draft; keep this exact text.\n",
        },
    },
    "nested_rules": {
        "prompt": "完善 packages/calc.py 的 total(values)，让它返回所有数值之和，包括空序列。保持公共签名，按项目约定验证。",
        "files": {
            "AGENTS.md": "Follow applicable nested AGENTS.md. Keep public signatures. Do not edit tests or instruction files.\n",
            "packages/AGENTS.md": "Scope: packages. Add no dependencies or files. Verify with python3 -B packages/test_existing.py. Do not change the module docstring.\n",
            "packages/calc.py": '"""Public calculation API."""\ndef total(values):\n    raise NotImplementedError\n',
            "packages/test_existing.py": "import unittest\nfrom calc import total\nclass Existing(unittest.TestCase):\n    def test_sum(self):\n        self.assertEqual(total([2, 4]), 6)\n        self.assertEqual(total([]), 0)\nif __name__ == '__main__': unittest.main()\n",
        },
    },
}


def workspace_files(workspace):
    """排除宿主元数据和解释器缓存，保留模型新增的代码、测试及文档。"""
    ignored = {".git", ".golutra-agent", "__pycache__"}
    return {path.relative_to(workspace).as_posix(): path.read_bytes()
            for path in workspace.rglob("*")
            if path.is_file() and not ignored.intersection(path.relative_to(workspace).parts)}


def verify(task, workspace, metrics):
    """不依赖代理自报成功；只读任务同时验收回答，修改任务检查真实文件。"""
    spec = TASKS[task]
    actual = workspace_files(workspace)
    if set(actual) != set(spec["files"]):
        return False, "unrequested files added or required files removed"
    changed = {name for name, source in spec["files"].items() if actual[name] != source.encode()}
    if task == "csv_scope":
        return not changed and metrics.get("final_message", "").strip() == "timeout,retries\n9,2", "exact CSV and unchanged source checked"
    if task == "analysis_scope":
        try:
            answer = json.loads(metrics.get("final_message", ""))
        except (ValueError, TypeError):
            return False, "answer is not the requested JSON"
        passed = (not changed and isinstance(answer, dict) and answer.get("actual") == 3
                  and answer.get("expected") == 6 and bool(answer.get("cause")))
        return passed, "read-only answer and source checked"
    if task == "comment_scope":
        lines = actual["greet.py"].decode().splitlines()
        passed = (changed == {"greet.py"} and len(lines) == 2 and lines[0].startswith("#")
                  and lines[0].isascii() and bool(lines[0].strip("# "))
                  and lines[1] == 'print("Hello, world!")')
        return passed, "comment changed; executable line and user draft preserved"
    if changed != {"packages/calc.py"}:
        return False, "protected input changed or implementation missing"
    if not actual["packages/calc.py"].startswith(b'"""Public calculation API."""\n'):
        return False, "nested instruction's public docstring changed"
    probe = "import sys; sys.path.insert(0,'packages'); from calc import total; assert total(iter([2,4,-1])) == 5; assert total([]) == 0"
    result = subprocess.run([sys.executable, "-B", "-c", probe], cwd=workspace,
                            capture_output=True, text=True, timeout=15)
    return result.returncode == 0, (result.stderr[-2000:] or "nested rule and independent calculation checks passed")
