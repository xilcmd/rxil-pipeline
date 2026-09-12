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
    # Two registered shows so `xil use` has something to list and switch to.
    for slug, show in (("the413", {"show": "THE 413", "season": 1}), ("nightowls", {"show": "Night Owls"})):
        d = WORKSPACE_FIXTURE / "configs" / slug
        d.mkdir(parents=True, exist_ok=True)
        (d / "project.json").write_text(json.dumps(show, indent=2) + "\n", encoding="utf-8")
    # Parsed JSONs, straight from the Python parser, so read-only commands
    # (episode-summary, parsed-csv, status...) have real input to chew on.
    env = dict(os.environ, XIL_PROJECTROOT=str(WORKSPACE_FIXTURE))
    env.pop("ELEVENLABS_API_KEY", None)
    for md in sorted(scripts.glob("*.md")):
        r = _run([str(PY_XIL), "parse", str(md.relative_to(WORKSPACE_FIXTURE)), "--quiet"], cwd=WORKSPACE_FIXTURE, env=env)
        if r.returncode != 0:
            print(f"parse failed for {md.name}:\n{r.stdout}{r.stderr}", file=sys.stderr)
    shutil.rmtree(WORKSPACE_FIXTURE / "logs", ignore_errors=True)
    # Historical logs in all three on-disk formats for stem-log. Seq/speaker
    # pairs 2/host, 3/host and 11/caller exist in parsed/mypodcast so --audit
    # gets one OK, one flagged and one unmatched record.
    logs = WORKSPACE_FIXTURE / "logs"
    logs.mkdir()
    (logs / "xil_v2_2026-08-01_hibirdy.log").write_text(
        '2026-08-01T10:00:00-0400|RUN|hibirdy|produce|BEGIN argv="xil produce --episode S01E01" pid=1 ver=0.3.2 cwd=/x\n'
        "2026-08-01T10:00:01-0400|INFO|hibirdy|produce|  > [002] host via Chatterbox Turbo (58 chars)...\n"
        "2026-08-01T10:00:05-0400|INFO|hibirdy|produce|   Saved: stems/mypodcast/S01E01/002_cold-open_host.mp3\n"
        "2026-08-01T10:00:05-0400|INFO|hibirdy|produce|   SHA256: 1111aaaa\n"
        "2026-08-01T10:00:06-0400|INFO|hibirdy|produce|  > [003] host with eleven_v3 (60 chars)...\n"
        "2026-08-01T10:00:09-0400|INFO|hibirdy|produce|   Saved: stems/mypodcast/S01E01/003_cold-open_host.mp3\n"
        "2026-08-01T10:00:09-0400|INFO|hibirdy|produce|   SHA256: 2222bbbb\n"
        "2026-08-01T10:00:10-0400|INFO|hibirdy|produce|  | speaker | lines |\n"
        "2026-08-01T10:00:11-0400|RUN|hibirdy|produce|END elapsed=9.0s\n"
        '2026-08-01T11:00:00-0400|RUN|hibirdy|produce|BEGIN argv="xil produce --episode S01E02" pid=2 ver=0.3.2 cwd=/x\n'
        "2026-08-01T11:00:01-0400|INFO|hibirdy|produce|  > [011] caller via gTTS (90 chars)...\n"
        "2026-08-01T11:00:02-0400|INFO|hibirdy|produce|   Saved: stems/mypodcast/S01E02/011_act1_caller.mp3\n"
        "2026-08-01T11:00:02-0400|INFO|hibirdy|produce|   SHA256: 3333cccc\n"
        "2026-08-01T11:00:03-0400|INFO|hibirdy|produce|  > [004] maya via chatterbox (50 chars)...\n"
        "2026-08-01T11:00:04-0400|INFO|hibirdy|produce|   Saved: stems/nightowls/S01E02/004_cold-open_maya.mp3\n",
        encoding="utf-8",
    )
    (logs / "xil_v1_2026-07-15.log").write_text(
        "--- Phase 1: Generating voices ---\n"
        "  > [001] adam with eleven_v3 (10 chars)...\n"
        "   Saved: stems/the413/S01E01/001_cold-open_adam.mp3\n"
        "   SHA256: aaaa\n"
        "  a v1 line | with | pipes\n"
        "--- Phase 1: Generating voices ---\n"
        "  > [002] adam with eleven_v3 (20 chars)...\n"
        "   Saved: stems/the413/S01E01/002_cold-open_adam.mp3\n"
        "   SHA256: bbbb\n",
        encoding="utf-8",
    )
    (logs / "xil_2026-07-10.log").write_text(
        "--- Phase 1: Generating voices ---\n"
        "  > [005] maya via Chatterbox (33 chars)...\n"
        "   Saved: stems/nightowls/S02E01/005_act1_maya.mp3\n"
        "   SHA256: cccc\n",
        encoding="utf-8",
    )
    (logs / "notes.txt").write_text("not a log\n")
    # A pre-0.1.8 flat workspace for migrate-workspace, tucked in a subdir
    # so it can be passed via --workspace without disturbing the rest.
    legacy = WORKSPACE_FIXTURE / "legacy"
    for d in ("parsed", "daw/S01E01", "masters", "cues", "configs/oldshow"):
        (legacy / d).mkdir(parents=True)
    (legacy / "project.json").write_text('{"show": "Old Show"}\n')
    for f in (
        "speakers.json",
        "cast_oldshow_S01E01.json",
        "sfx_oldshow_S01E01.json",
        "cast_oldshow_S01E02.json",
        "parsed/parsed_oldshow_S01E01.json",
        "parsed/parsed_oldshow_S01E01.csv",
        "parsed/parsed_oldshow_S01E01_annotated.csv",
        "parsed/orig_parsed_oldshow_S01E01.json",
        "parsed/pre_splice_parsed_oldshow_S01E01.json",
        "parsed/unrelated.txt",
        "daw/S01E01/S01E01_layer_dialogue.wav",
        "oldshow_S01E01_master.mp3",
        "masters/oldshow_S01E02_master.mp3",
        "cues/cues_oldshow_S01E01.md",
        "cues/cues_manifest_S01E01.json",
        "configs/oldshow/cast_S01E02.json",
    ):
        (legacy / f).write_text(f"legacy {f}\n")
    # A full artifact chain for `status`, with pinned mtimes so the freshness
    # verdicts are deterministic: gdoc newer than script (script STALE),
    # sfx config newer than daw (daw STALE), everything else in order.
    t0 = 1_700_000_000
    ws = WORKSPACE_FIXTURE
    chain = {
        "scripts/sample_S01E01.md": t0,
        "parsed/mypodcast/parsed_S01E01.json": t0 + 10,
        "stems/mypodcast/S01E01/002_cold-open_host.mp3": t0 + 20,
        "stems/mypodcast/S01E01/003_cold-open_host.mp3": t0 + 21,
        "stems/mypodcast/S01E01/S01E01_stem_manifest.json": t0 + 25,
        "daw/mypodcast/S01E01/S01E01_layer_dialogue.wav": t0 + 30,
        "configs/mypodcast/sfx_S01E01.json": t0 + 35,
        "masters/S01E01_mypodcast_2026-09-01.mp3": t0 + 40,
        "gdocs/S01E01_my_podcast.md.gdoc": t0 + 50,
        "gdocs/draft S01E04 notes.gdoc": t0 + 50,
    }
    sfx_cfg = {
        "show": "My Podcast", "season": 1, "episode": 1,
        "defaults": {"prompt_influence": 0.3},
        "effects": {
            "SFX: REJECTED THING": {"prompt": "a rejected sound", "duration_seconds": 2.0},
            "SFX: GOOD THING": {"prompt": "a good sound", "duration_seconds": 2.0},
            "BEAT": {"type": "silence", "duration_seconds": 1.0},
            "INTRO MUSIC": {"source": "SFX/beat.mp3"},
        },
    }
    for rel, mt in chain.items():
        p = ws / rel
        if not p.exists():
            p.parent.mkdir(parents=True, exist_ok=True)
            if rel.endswith("sfx_S01E01.json"):
                p.write_text(json.dumps(sfx_cfg, indent=2) + "\n", encoding="utf-8")
            elif rel.endswith(".json"):
                p.write_text("{}\n")
            else:
                p.write_bytes(b"fixture " + rel.encode())
        os.utime(p, (mt, mt))
    # Graded shared SFX assets: ID3 TXXX:XIL_GRADE frames written by mutagen
    # from the Python venv, exactly as xil-gui would. GOOD THING is rejected
    # only through its backend-tagged variant.
    py = PY_XIL.parent / "python"
    graded = {
        "SFX/sfx_rejected-thing.mp3": "rejected",
        "SFX/sfx_good-thing.mp3": "accurate",
        "SFX/sfx_good-thing.audioldm2.mp3": "rejected",
    }
    for rel, grade in graded.items():
        p = ws / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(b"")
        r = _run([str(py), "-c",
                  "import sys; from mutagen.id3 import ID3, TXXX; t = ID3(); "
                  "t.add(TXXX(encoding=3, desc='XIL_GRADE', text=sys.argv[2])); t.save(sys.argv[1])",
                  str(p), grade])
        if r.returncode != 0:
            print(f"grade tagging failed for {rel}:\n{r.stderr}", file=sys.stderr)
        os.utime(p, (t0, t0))
    # A tiny "SFX library" for mp3-hash: content is irrelevant, only the
    # extension and the directory shape matter.
    sfx = WORKSPACE_FIXTURE / "SFX"
    (sfx / "sub").mkdir(parents=True)
    (sfx / "beat.mp3").write_bytes(b"ID3\x03\x00beat")
    (sfx / "Door Slam.mp3").write_bytes(b"ID3\x03\x00door")
    (sfx / "sub" / "rain.MP3").write_bytes(b"ID3\x03\x00rain")
    (sfx / "notes.txt").write_text("not audio\n")
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
    env["XIL_TRACE_IMPL"] = "1"
    env.pop("ELEVENLABS_API_KEY", None)  # never let a parity run spend credits
    if force_py:
        env["XIL_FORCE_PY"] = "all"
    else:
        env.pop("XIL_FORCE_PY", None)
    proc = _run([str(binary), *args], cwd=ws, env=env)
    return ws, proc


def _impl_of(proc: subprocess.CompletedProcess) -> str:
    """Which implementation the Rust binary reported for this run."""
    for line in proc.stderr.splitlines():
        if line.startswith("rxil-impl: "):
            return line.split(": ", 1)[1].strip()
    return "unknown"


def _native_commands() -> set[str]:
    """Commands the Rust binary claims to implement natively.

    Raises when the binary is missing or too old to answer, so a stale or
    unbuilt binary fails the run instead of quietly making every native
    assertion vacuous.
    """
    if not RUST_XIL.is_file():
        raise SystemExit(f"Rust binary not found at {RUST_XIL} — run: cargo build --workspace")
    r = _run([str(RUST_XIL), "--native-list"])
    if r.returncode != 0:
        raise SystemExit(
            f"{RUST_XIL} does not support --native-list (exit {r.returncode}).\n"
            "The binary is stale. Rebuild it: cargo build --workspace"
        )
    return {line.strip() for line in r.stdout.splitlines() if line.strip()}


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
    # Identical bytes are identical whatever the type — this is every
    # untouched fixture file, so skip the typed comparators (and their
    # ffmpeg/numpy needs) for them.
    if a.stat().st_size == b.stat().st_size and a.read_bytes() == b.read_bytes():
        return None
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


def run_check(entry: dict, native: set[str]) -> tuple[list[str], str]:
    """Run one suite entry; return (failures, implementation that served the Rust side)."""
    args = entry["args"]
    mask_keys = set(entry.get("mask_keys", []))
    stdout_masks = [re.compile(p) for p in entry.get("stdout_masks", [])]

    ws_py, py = _run_side("py", RUST_XIL if entry.get("via_rust_shim", True) else PY_XIL, args, force_py=True)
    ws_rs, rs = _run_side("rs", RUST_XIL, args, force_py=False)

    failures = []
    impl = _impl_of(rs)
    # args[0] is the subcommand, unless the invocation is a bare flag like
    # `--help`, which the dispatcher answers itself and never routes.
    command = args[0] if args and not args[0].startswith("-") else ""
    # A check on a native command that silently fell back to Python proves
    # nothing — a stale binary or a failed build must not read as a pass.
    if command and impl == "unknown":
        failures.append("the Rust binary did not report which implementation ran (stale binary — rebuild)")
    elif command in native and impl != "native":
        failures.append(f"expected the native implementation of '{command}', but the Rust side ran: {impl}")
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
    return failures, impl


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
    native = _native_commands()
    failed = 0
    counts = {"native": 0, "delegated": 0, "unknown": 0}
    for entry in suite:
        if wanted is not None and entry["name"] not in wanted:
            continue
        failures, impl = run_check(entry, native)
        if not (entry["args"] and not entry["args"][0].startswith("-")):
            impl = "dispatcher"  # a bare flag, answered before any command runs
        counts[impl] = counts.get(impl, 0) + 1
        status = "PASS" if not failures else "FAIL"
        print(f"[{status}] {entry['name']} ({impl}): xil {' '.join(entry['args'])}")
        for f in failures:
            print(f"        {f}")
        failed += bool(failures)
    tally = ", ".join(f"{n} {k}" for k, n in counts.items() if n)
    print(f"Rust side: {tally}")
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
