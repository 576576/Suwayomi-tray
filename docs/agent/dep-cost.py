"""Analyze Cargo.lock: which packages exist only because of a given direct dep."""
import sys, collections

LOCK = "Cargo.lock"

def parse(path):
    pkgs = {}          # key -> {"deps": [key]}
    cur = None
    section = None
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if line == "[[package]]":
                cur = {"deps": [], "version": "", "source": None}
                section = None
                continue
            if line.startswith("[["):
                section = line
                continue
            if cur is None:
                continue
            if line.startswith("name = ") and "version" not in cur:
                cur["name"] = line.split('"')[1]
            elif line.startswith("version = "):
                cur["version"] = line.split('"')[1]
            elif line.startswith("source = "):
                cur["source"] = line.split('"')[1]
            elif line.startswith("dependencies = ["):
                section = "deps"
                continue
            elif line == "]" and section == "deps":
                section = None
                continue
            elif section == "deps":
                dep = line.strip().strip(",").strip('"')
                if dep:
                    cur["deps"].append(dep)
            elif line == "" and "name" in cur and cur not in pkgs.values():
                pass
    return pkgs

# Simpler hand-rolled parser
def parse2(path):
    pkgs = {}
    order = []
    cur = None
    in_deps = False
    with open(path, encoding="utf-8") as f:
        for line in f:
            s = line.strip()
            if s == "[[package]]":
                cur = {"deps": []}
                in_deps = False
                continue
            if cur is None:
                continue
            if s.startswith("name = "):
                cur["name"] = s.split('"')[1]
            elif s.startswith("version = "):
                cur["version"] = s.split('"')[1]
                key = f'{cur["name"]}@{cur["version"]}'
                pkgs[key] = cur
                order.append(key)
            elif s.startswith("dependencies = ["):
                in_deps = True
            elif s == "]":
                in_deps = False
            elif in_deps:
                cur["deps"].append(s.strip(",").strip('"'))
    return pkgs

pkgs = parse2(LOCK)
by_name = collections.defaultdict(list)
for k, v in pkgs.items():
    by_name[v["name"]].append(k)

def resolve(d):
    # dep string may be "name" or "name ver" (Cargo.lock v3+)
    if " " in d:
        name, ver = d.rsplit(" ", 1)
        key = f"{name}@{ver}"
        return key if key in pkgs else None
    cands = by_name.get(d)
    if not cands:
        return None
    return cands[0]

ROOT = "suwayomi-tray@0.1.0"
if ROOT not in pkgs:
    ROOT = [k for k in pkgs if k.startswith("suwayomi-tray")][0]

direct = pkgs[ROOT]["deps"]
print(f"total packages in lock: {len(pkgs)}")
print(f"direct deps: {len(direct)}")
print()

# reachability from root
def reach(start):
    seen = set()
    stack = [start]
    while stack:
        n = stack.pop()
        if n in seen:
            continue
        seen.add(n)
        for d in pkgs.get(n, {}).get("deps", []):
            r = resolve(d)
            if r and r not in seen:
                stack.append(r)
    return seen

all_reach = reach(ROOT)
print(f"packages reachable from root: {len(all_reach)}")
orphans = set(pkgs) - all_reach
print(f"unreachable (unused) packages in lock: {len(orphans)}")
for o in sorted(orphans):
    print("   ", o)
print()

direct_set = [resolve(d) for d in direct]
direct_set = [d for d in direct_set if d]

print("=== counterfactual: packages removed from lock if dep dropped ===")
def reach_without(exclude):
    seen = set()
    stack = [ROOT]
    while stack:
        n = stack.pop()
        if n in seen:
            continue
        seen.add(n)
        for d in pkgs.get(n, {}).get("deps", []):
            r = resolve(d)
            if r and r not in seen and not (n == ROOT and r == exclude):
                stack.append(r)
    return seen

base = len(all_reach)
res = []
for d in direct_set:
    n = base - len(reach_without(d))
    res.append((n, d))
res.sort(reverse=True)
for n, d in res:
    print(f"  drop {d:<40} -> -{n} packages ({len(all_reach)-n} remain)")
print()

print("=== exclusive cost of each direct dep ===")
rows = []
for d in direct_set:
    sub = reach(d)
    others = set()
    for o in direct_set:
        if o == d:
            continue
        others |= reach(o)
    excl = (sub - others) - {d}
    rows.append((len(excl), d, sorted(excl)))
rows.sort(reverse=True)
for n, d, excl in rows:
    print(f"\n{d}: +{n} exclusive packages")
    if n and n <= 60:
        for e in excl:
            print("    -", e)
