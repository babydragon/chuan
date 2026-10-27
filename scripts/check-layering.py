#!/usr/bin/env python3
"""架构守卫：kernel 的依赖树不得出现 IO 实现。

用法: python3 scripts/check-layering.py
退出码 0 = 通过, 1 = 违规。
"""
import json
import subprocess
import sys

BANNED = {
    "kernel": {"reqwest", "teloxide-core", "rusqlite", "rmcp", "image"},
}

def main() -> int:
    out = subprocess.check_output(["cargo", "metadata", "--format-version", "1"])
    meta = json.loads(out)
    packages = {p["id"]: p["name"] for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    targets = {
        "kernel": "harness-kernel",
    }
    failures = []
    for role, crate_name in targets.items():
        pid = next((i for i, n in packages.items() if n == crate_name), None)
        if pid is None:
            failures.append(f"{crate_name} not found in workspace")
            continue
        seen, stack = set(), [pid]
        names = set()
        while stack:
            cur = stack.pop()
            if cur in seen:
                continue
            seen.add(cur)
            names.add(packages.get(cur, cur))
            for dep in nodes.get(cur, {}).get("deps", []):
                stack.append(dep["pkg"])
        bad = sorted(names & BANNED[role])
        if bad:
            failures.append(f"{crate_name} must not depend on: {', '.join(bad)}")
    if failures:
        for f in failures:
            print(f"LAYERING VIOLATION: {f}", file=sys.stderr)
        return 1
    print("layering OK")
    return 0

if __name__ == "__main__":
    sys.exit(main())
