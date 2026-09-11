#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Parity harness: prove a Rust `xil` command produces what the Python one does.

    parity.py record                 # seed fixtures/ from $XIL_CODEROOT samples + the413 configs
    parity.py check <name> [...]     # run one suite entry (or several) under both and diff
    parity.py check --suite          # run every entry in suite.toml

Only the standard library is required; numpy is imported lazily for audio.

Each check copies fixtures/workspace/ twice into a scratch dir on the local
disk, runs `xil <args>` once under Python (XIL_FORCE_PY=all) and once under
Rust, then compares exit code, stdout, and every file the command wrote:

  *.json            re-serialised with indent=2 and compared byte for byte
                    (key order and float formatting must match)
  *.csv / *.txt     exact bytes after CRLF normalisation
  *.wav             byte-identical after the RIFF header
  *.mp3             decoded via ffmpeg; duration within 1 ms, RMS within
                    0.1 dB, peak within 0.2 dB, 1 s envelope correlation > .999
  logs/*.log        message column only; timestamps and durations masked
  everything else   exact bytes

Volatile values (timestamps, hostnames, absolute paths) are masked before
comparison by the regexes in MASKS and the per-check `mask_keys` list.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
from collections import OrderedDict
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"
WORKSPACE_FIXTURE = FIXTURES / "workspace"
SUITE = HERE / "suite.toml"
SCRATCH = Path(os.environ.get("XIL_PARITY_SCRATCH", HERE / "scratch"))

CODEROOT = Path(os.environ.get("XIL_CODEROOT", "/mnt/c/Users/shaba/src/python/xil-pipeline"))
PY_XIL = Path(os.environ.get("XIL_PY_BIN", CODEROOT / "venv" / "bin" / "xil"))
RUST_XIL = Path(
    os.environ.get("XIL_RS_BIN", Path.home() / ".cargo-target" / "rxil" / "debug" / "xil")
)

MASKS = [
    (re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{4}"), "<TS>"),
    (re.compile(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}"), "<TS>"),
    (re.compile(r"elapsed=\d+(\.\d+)?s"), "elapsed=<N>s"),
    (re.compile(r"pid=\d+"), "pid=<PID>"),
    (re.compile(r"ver=\S+"), "ver=<VER>"),
]


def _mask(text: str) -> str:
    # The two scratch workspaces differ only by name; hide both.
    for side in ("py", "rs"):
        text = text.replace(str(SCRATCH / side), "<WS>")
    for rx, repl in MASKS:
        text = rx.sub(repl, text)
    return text


# ---------------------------------------------------------------- record


def cmd_record(_: argparse.Namespace) -> int:
    """Seed fixtures/workspace from the Python repo's samples and the413 configs."""
    if WORKSPACE_FIXTURE.exists():
        shutil.rmtree(WORKSPACE_FIXTURE)
    scripts = WORKSPACE_FIXTURE / "scripts"
    scripts.mkdir(parents=True)
    for md in sorted((CODEROOT / "samples").glob("*.md")):
        shutil.copy2(md, scripts / md.name)
    shutil.copy2(CODEROOT / "samples" / "project.json", WORKSPACE_FIXTURE / "project.json")
    src_cfg = CODEROOT / "configs" / "the413"
    if src_cfg.is_dir():
        shutil.copytree(src_cfg, WORKSPACE_FIXTURE / "configs" / "the413")
    manifest = {
        "python_git_sha": _git_sha(CODEROOT),
        "python_version": _run([str(PY_XIL), "--version"]).stdout.strip(),
        "files": sorted(str(p.relative_to(WORKSPACE_FIXTURE)) for p in WORKSPACE_FIXTURE.rglob("*") if p.is_file()),
    }
    (FIXTURES / "MANIFEST.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"recorded {len(manifest['files'])} files into {WORKSPACE_FIXTURE}")
    return 0


def _git_sha(repo: Path) -> str:
    r = subprocess.run(["git", "-C", str(repo), "rev-parse", "HEAD"], capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else "unknown"


# ---------------------------------------------------------------- check


def _run(argv: list[str], cwd: Path | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True)


def _fresh_workspace(side: str) -> Path:
    ws = SCRATCH / side
    if ws.exists():
        shutil.rmtree(ws)
    shutil.copytree(WORKSPACE_FIXTURE, ws)
    return ws


def _run_side(side: str, binary: Path, args: list[str], force_py: bool) -> tuple[Path, subprocess.CompletedProcess]:
    ws = _fresh_workspace(side)
    env = dict(os.environ)
    env["XIL_PROJECTROOT"] = str(ws)
    env["XIL_CODEROOT"] = str(CODEROOT)
    env.pop("ELEVENLABS_API_KEY", None)  # never let a parity run spend credits
    if force_py:
        env["XIL_FORCE_PY"] = "all"
    else:
        env.pop("XIL_FORCE_PY", None)
    proc = _run([str(binary), *args], cwd=ws, env=env)
    return ws, proc


def _snapshot(ws: Path) -> dict[str, Path]:
    return {str(p.relative_to(ws)): p for p in ws.rglob("*") if p.is_file()}


def _compare_json(a: Path, b: Path, mask_keys: set[str]) -> str | None:
    def load(p: Path):
        return json.loads(p.read_text(encoding="utf-8"), object_pairs_hook=OrderedDict)

    def scrub(obj):
        if isinstance(obj, dict):
            return OrderedDict((k, "<MASKED>" if k in mask_keys else scrub(v)) for k, v in obj.items())
        if isinstance(obj, list):
            return [scrub(v) for v in obj]
        if isinstance(obj, str):
            return _mask(obj)
        return obj

    try:
        da, db = scrub(load(a)), scrub(load(b))
    except json.JSONDecodeError as exc:
        return f"invalid JSON: {exc}"
    sa = json.dumps(da, indent=2, ensure_ascii=False, sort_keys=False)
    sb = json.dumps(db, indent=2, ensure_ascii=False, sort_keys=False)
    return None if sa == sb else _first_diff(sa, sb)


def _compare_text(a: Path, b: Path) -> str | None:
    ta = _mask(a.read_bytes().decode("utf-8", "replace").replace("\r\n", "\n"))
    tb = _mask(b.read_bytes().decode("utf-8", "replace").replace("\r\n", "\n"))
    return None if ta == tb else _first_diff(ta, tb)


def _compare_log(a: Path, b: Path) -> str | None:
    def messages(p: Path) -> list[str]:
        out = []
        for line in p.read_text(encoding="utf-8", errors="replace").splitlines():
            parts = line.split("|", 4)
            out.append(_mask(parts[4] if len(parts) == 5 else line))
        return out

    ma, mb = messages(a), messages(b)
    return None if ma == mb else _first_diff("\n".join(ma), "\n".join(mb))


def _compare_wav(a: Path, b: Path) -> str | None:
    def body(p: Path) -> bytes:
        data = p.read_bytes()
        i = data.find(b"data")
        return data[i + 8 :] if i >= 0 else data

    return None if body(a) == body(b) else "PCM payload differs"


def _compare_mp3(a: Path, b: Path) -> str | None:
    import numpy as np  # lazy: only audio checks need it

    def decode(p: Path):
        r = subprocess.run(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(p), "-f", "s16le", "-ac", "2", "-ar", "48000", "pipe:1"],
            capture_output=True,
        )
        return np.frombuffer(r.stdout, dtype=np.int16).reshape(-1, 2).astype(np.float64) / 32768.0

    xa, xb = decode(a), decode(b)
    if abs(len(xa) - len(xb)) > 48:  # 1 ms at 48 kHz
        return f"length differs: {len(xa)} vs {len(xb)} frames"
    n = min(len(xa), len(xb))
    xa, xb = xa[:n], xb[:n]
    def db(x: float) -> float:
        return 20 * np.log10(max(x, 1e-12))

    for ch in range(2):
        rms_a, rms_b = np.sqrt(np.mean(xa[:, ch] ** 2)), np.sqrt(np.mean(xb[:, ch] ** 2))
        if abs(db(rms_a) - db(rms_b)) > 0.1:
            return f"ch{ch} RMS {db(rms_a):.2f} vs {db(rms_b):.2f} dB"
        pk_a, pk_b = np.max(np.abs(xa[:, ch])), np.max(np.abs(xb[:, ch]))
        if abs(db(pk_a) - db(pk_b)) > 0.2:
            return f"ch{ch} peak {db(pk_a):.2f} vs {db(pk_b):.2f} dB"
    win = 48000
    env_a = np.array([np.sqrt(np.mean(xa[i : i + win] ** 2)) for i in range(0, n, win)])
    env_b = np.array([np.sqrt(np.mean(xb[i : i + win] ** 2)) for i in range(0, n, win)])
    if len(env_a) > 2:
        corr = np.corrcoef(env_a, env_b)[0, 1]
        if corr < 0.999:
            return f"envelope correlation {corr:.4f}"
    return None


def _compare_file(rel: str, a: Path, b: Path, mask_keys: set[str]) -> str | None:
    suffix = a.suffix.lower()
    if rel.startswith("logs/") and suffix == ".log":
        return _compare_log(a, b)
    if suffix == ".json":
        return _compare_json(a, b, mask_keys)
    if suffix in {".csv", ".txt", ".md", ".html", ".jsonl", ".py"}:
        return _compare_text(a, b)
    if suffix == ".wav":
        return _compare_wav(a, b)
    if suffix == ".mp3":
        return _compare_mp3(a, b)
    return None if a.read_bytes() == b.read_bytes() else "bytes differ"


def _first_diff(a: str, b: str) -> str:
    la, lb = a.splitlines(), b.splitlines()
    for i, (x, y) in enumerate(zip(la, lb)):
        if x != y:
            return f"line {i + 1}:\n    py: {x}\n    rs: {y}"
    return f"line count {len(la)} vs {len(lb)}"


def run_check(entry: dict) -> list[str]:
    """Run one suite entry; return a list of failure descriptions (empty = pass)."""
    args = entry["args"]
    mask_keys = set(entry.get("mask_keys", []))
    stdout_masks = [re.compile(p) for p in entry.get("stdout_masks", [])]

    ws_py, py = _run_side("py", RUST_XIL if entry.get("via_rust_shim", True) else PY_XIL, args, force_py=True)
    ws_rs, rs = _run_side("rs", RUST_XIL, args, force_py=False)

    failures = []
    if py.returncode != rs.returncode:
        failures.append(f"exit code: py={py.returncode} rs={rs.returncode}")

    def norm(text: str) -> str:
        text = _mask(text)
        for rx in stdout_masks:
            text = rx.sub("<MASK>", text)
        return text

    if norm(py.stdout) != norm(rs.stdout):
        failures.append("stdout differs: " + _first_diff(norm(py.stdout), norm(rs.stdout)))

    files_py, files_rs = _snapshot(ws_py), _snapshot(ws_rs)
    for rel in sorted(set(files_py) - set(files_rs)):
        failures.append(f"only Python wrote: {rel}")
    for rel in sorted(set(files_rs) - set(files_py)):
        failures.append(f"only Rust wrote: {rel}")
    for rel in sorted(set(files_py) & set(files_rs)):
        why = _compare_file(rel, files_py[rel], files_rs[rel], mask_keys)
        if why:
            failures.append(f"{rel}: {why}")
    return failures


def cmd_check(ns: argparse.Namespace) -> int:
    if not WORKSPACE_FIXTURE.exists():
        print("no fixtures; run `parity.py record` first", file=sys.stderr)
        return 2
    suite = tomllib.loads(SUITE.read_text())["check"]
    wanted = None if ns.suite else set(ns.names)
    if wanted is not None:
        missing = wanted - {e["name"] for e in suite}
        if missing:
            print(f"unknown check(s): {', '.join(sorted(missing))}", file=sys.stderr)
            return 2
    failed = 0
    for entry in suite:
        if wanted is not None and entry["name"] not in wanted:
            continue
        failures = run_check(entry)
        status = "PASS" if not failures else "FAIL"
        print(f"[{status}] {entry['name']}: xil {' '.join(entry['args'])}")
        for f in failures:
            print(f"        {f}")
        failed += bool(failures)
    return 1 if failed else 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("record", help="seed fixtures/ from the Python repo").set_defaults(fn=cmd_record)
    c = sub.add_parser("check", help="run parity checks")
    c.add_argument("names", nargs="*", help="check names from suite.toml")
    c.add_argument("--suite", action="store_true", help="run every check")
    c.set_defaults(fn=cmd_check)
    ns = p.parse_args()
    if ns.cmd == "check" and not ns.suite and not ns.names:
        p.error("give check names or --suite")
    return ns.fn(ns)


if __name__ == "__main__":
    sys.exit(main())
