#!/usr/bin/env python3
"""Audit of a rust-pure workspace.

Checks:
  1. Purity: no crates that compile bundled C/C++ (a deny list +
     cc/cmake/bindgen build dependencies), based on `cargo metadata`.
  2. Layers: every internal crate depends only on crates of a strictly
     lower layer (layers.toml), except for explicit [allow] exceptions.
  3. Crate hygiene: `[lints] workspace = true` and `#![forbid(unsafe_code)]`
     (except for crates in unsafe_allowed).
  4. Per-file `unsafe` count against `[unsafe_budget]` in layers.toml; with
     `--unsafe-baseline=<layers.toml of the base revision>`, growth requires a new DECISIONS.md entry.

Usage:
  python3 audit_workspace.py <workspace-dir> [--no-metadata] [--all-features]
                             [--targets=x86_64-unknown-linux-gnu,wasm32-unknown-unknown]
                             [--unsafe-baseline=<file>]
Defaults: default features, all platforms at once (the strictest).
Exit code 1 on errors, 0 if clean (warnings do not fail the run).
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    sys.exit("Python 3.11+ is required (tomllib)")

# Crates that compile bundled C/C++ (or C/asm) code.
VENDORED_C = {
    "cc": "C/C++ compiler in a build script",
    "cmake": "C/C++ build via CMake",
    "bindgen": "bindings generated from C headers",
    "zstd-sys": "use ruzstd",
    "libz-sys": "use miniz_oxide",
    "lz4-sys": "use lz4_flex",
    "ring": "C/asm; for TLS use rustls-rustcrypto",
    "aws-lc-sys": "C",
    "aws-lc-fips-sys": "C",
    "openssl-sys": "C (OpenSSL); use rustls",
    "shaderc-sys": "use naga",
    "glslang-sys": "use naga",
    "spirv-cross-sys": "use naga",
    "sdl2-sys": "use winit",
    "sdl3-sys": "use winit",
    "glfw-sys": "use winit",
    "physx-sys": "use rapier",
    "mlua-sys": "bundled Lua; use rhai/wasmi",
    "lua-src": "bundled Lua",
    "luajit-src": "bundled LuaJIT",
    "tracy-client-sys": "C++; use puffin",
    "freetype-sys": "use swash/fontdue",
    "harfbuzz-sys": "use rustybuzz",
    "imgui-sys": "C++; use egui",
    "libwebp-sys": "C",
    "dav1d-sys": "C",
    "openal-sys": "use cpal/kira",
    "assimp-sys": "use gltf",
    "russimp-sys": "use gltf",
    "basis-universal-sys": "C++",
    "libsqlite3-sys": "C (SQLite)",
}
# Bindings to OS system libraries: acceptable, but they require dev packages.
SYSTEM_FFI = {
    "alsa-sys": "system libasound (apt: libasound2-dev)",
    "libudev-sys": "system libudev (apt: libudev-dev)",
    "pkg-config": "system library lookup",
}
# Tools outside the engine tree (fuzzing etc.) that can be ignored.
DEV_TOOL_OK = {"libfuzzer-sys"}


class Report:
    def __init__(self) -> None:
        self.errors: list[str] = []
        self.warnings: list[str] = []
        self.info: list[str] = []

    def print(self) -> int:
        for title, items, mark in (("ERRORS", self.errors, "✗"),
                                   ("Warnings", self.warnings, "!"),
                                   ("Info", self.info, "i")):
            if items:
                print(f"\n{title}:")
                for it in items:
                    print(f"  {mark} {it}")
        if not self.errors:
            print("\n✓ Audit passed" + (" (with warnings)" if self.warnings else ""))
            return 0
        print(f"\n✗ Errors: {len(self.errors)}")
        return 1


def load_toml(p: Path) -> dict:
    with p.open("rb") as f:
        return tomllib.load(f)


def workspace_members(root: Path) -> dict[str, Path]:
    ws = load_toml(root / "Cargo.toml").get("workspace", {})
    members: dict[str, Path] = {}
    for pattern in ws.get("members", []):
        for d in sorted(root.glob(pattern)):
            ct = d / "Cargo.toml"
            if ct.is_file():
                name = load_toml(ct).get("package", {}).get("name")
                if name:
                    members[name] = d
    return members


def internal_deps(manifest: dict, members: set[str]) -> set[str]:
    out: set[str] = set()
    sections = [manifest.get("dependencies", {}), manifest.get("build-dependencies", {})]
    for t in manifest.get("target", {}).values():
        sections += [t.get("dependencies", {}), t.get("build-dependencies", {})]
    for sec in sections:
        for key, val in sec.items():
            real = val.get("package", key) if isinstance(val, dict) else key
            if real in members:
                out.add(real)
    return out


def check_layers(root: Path, members: dict[str, Path], r: Report) -> dict:
    lp = root / "layers.toml"
    if not lp.is_file():
        r.warnings.append("layers.toml not found; layer check skipped")
        return {}
    cfg = load_toml(lp)
    layers: dict[str, int] = cfg.get("crates", {})
    allow: dict[str, list[str]] = cfg.get("allow", {})
    for name in members:
        if name not in layers:
            r.errors.append(f"{name}: not in layers.toml; assign it a layer")
    for name, d in members.items():
        if name not in layers:
            continue
        manifest = load_toml(d / "Cargo.toml")
        for dep in sorted(internal_deps(manifest, set(members))):
            if dep not in layers:
                continue
            if layers[dep] < layers[name] or dep in allow.get(name, []):
                continue
            r.errors.append(
                f"{name} (L{layers[name]}) depends on {dep} (L{layers[dep]}): "
                "an upward or sideways dependency across layers. Move the code lower or add "
                "an exception to [allow] with an entry in DECISIONS.md")
    return cfg


def check_hygiene(members: dict[str, Path], layers_cfg: dict, r: Report) -> None:
    unsafe_ok = set(layers_cfg.get("unsafe_allowed", []))
    for name, d in members.items():
        manifest = load_toml(d / "Cargo.toml")
        if not manifest.get("lints", {}).get("workspace"):
            r.errors.append(f"{name}: no `[lints] workspace = true`; strict lints are not applied")
        roots = [p for p in (d / "src" / "lib.rs", d / "src" / "main.rs") if p.is_file()]
        for rp in roots:
            text = rp.read_text(encoding="utf-8", errors="replace")
            has_forbid = "#![forbid(unsafe_code)]" in text
            if name in unsafe_ok:
                if "undocumented_unsafe_blocks" not in text and "#![deny(unsafe_op_in_unsafe_fn)]" not in text:
                    r.warnings.append(f"{name}: unsafe is allowed; add #![deny(unsafe_op_in_unsafe_fn)] "
                                      "and run the tests under Miri")
            elif not has_forbid:
                r.errors.append(f"{name}: {rp.relative_to(d)} lacks #![forbid(unsafe_code)] "
                                "(or add the crate to unsafe_allowed in layers.toml)")


def check_purity(root: Path, members: set[str], r: Report, accepted: dict[str, str]) -> None:
    try:
        proc = subprocess.run(
            ["cargo", "metadata", "--format-version", "1"]
            + (["--all-features"] if ALL_FEATURES else [])
            + [f"--filter-platform={t}" for t in TARGETS],
            cwd=root, capture_output=True, text=True, timeout=600)
    except FileNotFoundError:
        r.warnings.append("cargo not found; purity check skipped")
        return
    except subprocess.TimeoutExpired:
        r.warnings.append("cargo metadata: timeout; purity check skipped")
        return
    if proc.returncode != 0:
        tail = proc.stderr.strip().splitlines()[-3:]
        r.warnings.append("cargo metadata failed (no network?); purity check skipped: "
                          + " | ".join(tail))
        return
    meta = json.loads(proc.stdout)
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in (meta.get("resolve") or {}).get("nodes", [])}

    # reverse graph for "who pulls it in" chains
    parents: dict[str, set[str]] = {}
    for nid, n in nodes.items():
        for d in n.get("deps", []):
            parents.setdefault(d["pkg"], set()).add(nid)

    def chain(pid: str) -> str:
        path, cur, seen = [pkgs[pid]["name"]], pid, {pid}
        while True:
            ps = [p for p in parents.get(cur, ()) if p not in seen]
            if not ps:
                break
            ws = [p for p in ps if pkgs[p]["name"] in members]
            cur = (ws or sorted(ps))[0]
            seen.add(cur)
            path.append(pkgs[cur]["name"])
            if pkgs[cur]["name"] in members:
                break
        return " ← ".join(path)

    found_c, found_sys = [], []
    c_tools = {"cc", "cmake", "bindgen"}
    for pid in nodes:
        p = pkgs[pid]
        name = p["name"]
        if name in members or name in DEV_TOOL_OK or name in c_tools:
            continue
        # build dependencies actually activated in the resolve graph
        bdeps = {pkgs[d["pkg"]]["name"] for d in nodes[pid].get("deps", [])
                 if any(k.get("kind") == "build" for k in d.get("dep_kinds", []))}
        uses_c = sorted(bdeps & c_tools)
        if name in VENDORED_C:
            found_c.append(f"{name} {p['version']}: {VENDORED_C[name]} — chain: {chain(pid)}")
        elif uses_c:
            found_c.append(f"{name} {p['version']}: build.rs invokes {', '.join(uses_c)} "
                           f"— chain: {chain(pid)}")
        elif name.endswith("-src"):
            found_c.append(f"{name} {p['version']}: looks like vendored C/C++ sources — chain: {chain(pid)}")
        elif name in SYSTEM_FFI:
            found_sys.append(f"{name}: {SYSTEM_FFI[name]} — chain: {chain(pid)}")
        elif name.endswith("-sys"):
            found_sys.append(f"{name}: FFI bindings without building C (check that this is a system "
                             f"library) — chain: {chain(pid)}")
    def dedupe(xs):
        seen, out = set(), []
        for x in sorted(xs):
            key = x.split(" ", 1)[0].rstrip(":")
            if key not in seen:
                seen.add(key)
                out.append(x)
        return out
    found_c, found_sys = dedupe(found_c), dedupe(found_sys)
    for x in found_c:
        crate = x.split(" ", 1)[0]
        if crate in accepted:
            r.warnings.append(f"accepted C layer: {x} (reason: {accepted[crate]})")
        else:
            r.errors.append(f"bundled C/C++: {x}")
    found_c = []
    r.errors += [f"bundled C/C++: {x}" for x in found_c]
    r.info += [f"system FFI: {x}" for x in found_sys]
    scope = ", ".join(TARGETS) if TARGETS else "all platforms"
    feats = "all features" if ALL_FEATURES else "default features"
    r.info.append(f"packages checked in the graph: {len(nodes)} ({scope}; {feats})")


def strip_comments_and_strings(src: str) -> str:
    """Code without comments, string and char literals (for keyword search)."""
    out, i, n = [], 0, len(src)
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            i = n if j < 0 else j
        elif src.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif src.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        elif c == "r" and (src.startswith('r"', i) or src.startswith('r#', i)) and \
                (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            j = i + 1
            while j < n and src[j] == "#":
                j += 1
            if j < n and src[j] == '"':
                end = src.find('"' + "#" * (j - i - 1), j + 1)
                i = n if end < 0 else end + 1 + (j - i - 1)
            else:
                out.append(c)
                i += 1
        elif c == '"':
            i += 1
            while i < n and src[i] != '"':
                i += 2 if src[i] == "\\" else 1
            i += 1
        elif c == "'":
            # A char literal ('x', '\n', '\u{..}') or a lifetime ('a); lifetimes are kept.
            if src.startswith("\\", i + 1):
                end = src.find("'", i + 2)
                i = n if end < 0 else end + 1
            elif i + 2 < n and src[i + 2] == "'":
                i += 3
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


UNSAFE_WORD = re.compile(r"\bunsafe\b")


def count_unsafe(root: Path, members: dict[str, Path]) -> dict[str, int]:
    """Occurrences of the `unsafe` keyword in code (blocks, `unsafe fn`/`impl`,
    `unsafe fn(..)` types) per file across all workspace crates and `fuzz/`."""
    dirs = list(members.values()) + ([root / "fuzz"] if (root / "fuzz").is_dir() else [])
    counts: dict[str, int] = {}
    for d in dirs:
        for path in sorted(d.rglob("*.rs")):
            rel = path.relative_to(root)
            if "target" in rel.parts:
                continue
            code = strip_comments_and_strings(path.read_text(encoding="utf-8", errors="replace"))
            hits = len(UNSAFE_WORD.findall(code))
            if hits:
                counts[rel.as_posix()] = hits
    return counts


def check_unsafe_budget(root: Path, members: dict[str, Path], layers_cfg: dict,
                        baseline_cfg: dict | None, r: Report) -> None:
    """The `unsafe` count against `[unsafe_budget]` in layers.toml (docs/reliability.md §3).

    The budget is exact: growth or shrinkage without editing the budget is an error. Every entry
    references DECISIONS.md decisions; if a baseline is given (layers.toml of the base revision),
    growth of a file's count requires a decision that the file did not have in the baseline.
    """
    budget: dict[str, dict] = layers_cfg.get("unsafe_budget", {})
    counts = count_unsafe(root, members)
    decisions_text = (root / "DECISIONS.md").read_text(encoding="utf-8") \
        if (root / "DECISIONS.md").is_file() else ""
    known = {int(m) for m in re.findall(r"^\|\s*(\d+)\s*\|", decisions_text, re.M)}
    base_budget: dict[str, dict] = (baseline_cfg or {}).get("unsafe_budget", {})
    unsafe_ok = set(layers_cfg.get("unsafe_allowed", []))
    for rel in sorted(set(counts) | set(budget)):
        have = counts.get(rel, 0)
        entry = budget.get(rel)
        if entry is None:
            r.errors.append(f"unsafe: {rel} — {have} occurrences without a budget. Add an entry to "
                            "DECISIONS.md and a line to [unsafe_budget] in layers.toml")
            continue
        want = int(entry.get("count", 0))
        refs = [int(x) for x in entry.get("decisions", [])]
        if not refs:
            r.errors.append(f"unsafe: {rel} — the budget has no DECISIONS.md reference")
        for ref in refs:
            if ref not in known:
                r.errors.append(f"unsafe: {rel} references DECISIONS #{ref}, which does not exist")
        if have > want:
            r.errors.append(f"unsafe: {rel} — {have} occurrences, budget {want}. Growth only with "
                            "a new DECISIONS.md entry and an [unsafe_budget] update")
        elif have < want:
            r.errors.append(f"unsafe: {rel} — {have} occurrences, budget {want}: lower the budget")
        crate = next((n for n, d in members.items()
                      if (root / rel).resolve().is_relative_to(d.resolve())), None)
        is_test = any(p in ("tests", "benches") for p in Path(rel).parts)
        if crate and crate not in unsafe_ok and not is_test and have:
            r.errors.append(f"unsafe: {rel} — crate {crate} is not in unsafe_allowed")
        if baseline_cfg is not None:
            old = base_budget.get(rel, {})
            if want > int(old.get("count", 0)):
                new_refs = set(refs) - {int(x) for x in old.get("decisions", [])}
                if not new_refs:
                    r.errors.append(
                        f"unsafe: {rel} — the budget grew {old.get('count', 0)} → {want} without a new "
                        "DECISIONS.md entry in `decisions`")
    total = sum(counts.values())
    r.info.append(f"unsafe: {total} occurrences in {len(counts)} files (layers.toml budget)")


ALL_FEATURES = False
TARGETS: list[str] = []


def main() -> None:
    global ALL_FEATURES, TARGETS
    ALL_FEATURES = "--all-features" in sys.argv
    for a in sys.argv[1:]:
        if a.startswith("--targets="):
            TARGETS = [t for t in a.split("=", 1)[1].split(",") if t]
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    root = Path(args[0] if args else ".").resolve()
    if not (root / "Cargo.toml").is_file():
        sys.exit(f"{root}: no Cargo.toml")
    r = Report()
    members = workspace_members(root)
    if not members:
        r.errors.append("workspace has no members")
    layers_cfg = check_layers(root, members, r)
    check_hygiene(members, layers_cfg, r)
    baseline_cfg = None
    for a in sys.argv[1:]:
        if a.startswith("--unsafe-baseline="):
            baseline_path = Path(a.split("=", 1)[1])
            baseline_cfg = load_toml(baseline_path) if baseline_path.stat().st_size else {}
    check_unsafe_budget(root, members, layers_cfg, baseline_cfg, r)
    if "--no-metadata" not in sys.argv:
        accepted = layers_cfg.get("purity", {}).get("accepted", {})
        check_purity(root, set(members), r, accepted)
    print(f"Audit of {root}: {len(members)} crates")
    sys.exit(r.print())


if __name__ == "__main__":
    main()
