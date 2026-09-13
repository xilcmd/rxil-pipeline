#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Parity harness: prove a Rust `xil` command produces what the Python one does.

    parity.py record                 # seed fixtures/ from $XIL_CODEROOT samples + the413 configs
    parity.py check <name> [...]     # run one suite entry (or several) under both and diff
    parity.py check --suite          # run every entry in suite.toml

Only the standard library is required; numpy is imported lazily for audio.

The reference implementation is the Python at $XIL_CODEROOT — its working
main, not the PyPI release. xil-pipeline's version string has said "0.3.2"
for many commits past the 0.3.2 tag, and the port reproduces the behaviour
of the code as it stands, not of the last release. CI pins the matching
commit; see .github/workflows/ci.yml.

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
    # Log records use +0000, the edit journal uses +00:00, and two runs a
    # second apart must still compare equal.
    (re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:[+-]\d{2}:?\d{2}|Z)"), "<TS>"),
    (re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{4}"), "<TS>"),
    (re.compile(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}"), "<TS>"),
    (re.compile(r"elapsed=\d+(\.\d+)?s"), "elapsed=<N>s"),
    # The run banner's console trailer: "  xil scan  |  finished … (0.1s)".
    (re.compile(r"\(\d+\.\d+s\)"), "(<N>s)"),
    (re.compile(r"pid=\d+"), "pid=<PID>"),
    # ffmpeg tags its own log lines with a heap address, e.g.
    # "[mp3 @ 0x583653da6300] …", which reaches the operator verbatim
    # inside a decode failure.
    (re.compile(r"@ 0x[0-9a-f]+\]"), "@ 0x<PTR>]"),
    (re.compile(r"ver=\S+"), "ver=<VER>"),
    # sfx-impact's HTML page stamps the minute and zone it was written.
    (re.compile(r"generated \d{4}-\d{2}-\d{2} \d{2}:\d{2} \S+"), "generated <STAMP>"),
    # The DAW timeline page stamps its render minute, and cache-busts each
    # audio URL with the file's mtime — layer WAVs written a moment apart.
    (re.compile(r"Generated \d{4}-\d{2}-\d{2} \d{2}:\d{2}"), "Generated <STAMP>"),
    (re.compile(r"\?v=\d+"), "?v=<MTIME>"),
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
    # A script exercising the parser corners the tidy samples never hit:
    # a CAST block, every BEAT shape, span markers, stop markers, pipe
    # hints good and bad, multi-line dialogue, a subtitled act header, an
    # unrecognised bracket, and an accented cue.
    (scripts / "torture_S02E05.md").write_text(
        'Torture Show Season 2: Episode 5: "Every Corner" Arc: "The Hard Parts"\n'
        "\n"
        "CAST:\n"
        "* NORA WALSH — Detective\n"
        "* T-BONE — Sidekick\n"
        "* ADAM — Host\n"
        "\n"
        "===\n"
        "\n"
        "COLD OPEN\n"
        "\n"
        "SCENE 1: THE BOOTH [AMBIENCE: room tone | tone.mp3]\n"
        "\n"
        "[SFX: DOOR OPENS | door.mp3 | play_volume_pct=20%]\n"
        "[MUSIC: STING | sting.mp3 | play_duration_pct=35]\n"
        "[AMBIENCE: RAIN | rain.mp3 | play_duration_pct=50]\n"
        "[SFX: BAD HINT | play_volume_pct=abc]\n"
        "[SFX: OUT OF RANGE | play_volume_pct=500]\n"
        "[SFX: CAFÉ MURMUR]\n"
        "[drawn out]\n"
        "[BEAT]\n"
        "[LONG BEAT]\n"
        "[BEAT — 3 SECONDS]\n"
        "[BEAT — LONG, 5 SECONDS]\n"
        "[AMBIENCE: STOP]\n"
        "[AMBIENCE: RAIN FADES OUT]\n"
        "[FILM AUDIO: ENGAGES]\n"
        "[PHONE FILTER: ENGAGES NORA WALSH]\n"
        "[SPEAKERPHONE: ENGAGES]\n"
        "[VINTAGE FILTER: ENGAGES]\n"
        "\n"
        "NORA WALSH (quietly)\n"
        "The first line.\n"
        "It continues here.\n"
        "(beat)\n"
        "And ends here.\n"
        "\n"
        "T-BONE\n"
        "[BEAT]\n"
        "Interrupted by a cue.\n"
        "\n"
        "ADAM (on the phone) Single line form.\n"
        "\n"
        "===\n"
        "\n"
        'ACT ONE: "Subtitled"\n'
        "\n"
        "SCENE 2A: ALLEY\n"
        "\n"
        "ADAM Second act line.\n"
        "\n"
        "===\n"
        "\n"
        "END OF EPISODE\n"
        "\n"
        "ADAM This line is past the end marker.\n",
        encoding="utf-8",
    )
    src_cfg = CODEROOT / "configs" / "the413"
    if src_cfg.is_dir():
        shutil.copytree(src_cfg, WORKSPACE_FIXTURE / "configs" / "the413")
    # Two registered shows so `xil use` has something to list and switch to.
    for slug, show in (("the413", {"show": "THE 413", "season": 1}), ("nightowls", {"show": "Night Owls"})):
        d = WORKSPACE_FIXTURE / "configs" / slug
        d.mkdir(parents=True, exist_ok=True)
        (d / "project.json").write_text(json.dumps(show, indent=2) + "\n", encoding="utf-8")
    # Two more scripts with pre-existing state beside them, so `xil parse`
    # takes its other two paths: backfill onto an existing sfx config, and
    # skeleton-then-journal-replay.
    for tag, name in (("S03E01", "backfill"), ("S04E01", "journal")):
        (scripts / f"{name}_{tag}.md").write_text(
            f'Hint Show Season {tag[2]}: Episode {int(tag[-2:])}: "Hints"\n'
            "\n"
            "CAST:\n"
            "* ADAM — Host\n"
            "\n"
            "===\n"
            "\n"
            "COLD OPEN\n"
            "\n"
            "[SFX: KEPT SOURCE | new.mp3]\n"
            "[SFX: FILLED SOURCE | filled.mp3]\n"
            "[SFX: BRAND NEW | brandnew.mp3 | play_volume_pct=40]\n"
            "[AMBIENCE: NEW BED | bed.mp3]\n"
            "[MUSIC: VOLUME ONLY | play_volume_pct=15]\n"
            "\n"
            "ADAM A line.\n",
            encoding="utf-8",
        )
    cfg_dir = WORKSPACE_FIXTURE / "configs" / "hintshow"
    cfg_dir.mkdir(parents=True, exist_ok=True)
    (cfg_dir / "project.json").write_text(json.dumps({"show": "Hint Show"}, indent=2) + "\n", encoding="utf-8")
    # Existing config for the backfill path: one source to keep, one to fill,
    # one stale piped key, one stub prompt that must be dropped.
    (cfg_dir / "sfx_S03E01.json").write_text(
        json.dumps(
            {
                "show": "Hint Show", "season": 3, "episode": 1,
                "defaults": {"prompt_influence": 0.3},
                "effects": {
                    "SFX: KEPT SOURCE": {"source": "SFX/already-here.mp3", "duration_seconds": 5.0},
                    "SFX: FILLED SOURCE": {"prompt": "SFX: FILLED SOURCE", "duration_seconds": 5.0},
                    "MUSIC: VOLUME ONLY": {"prompt": "MUSIC: VOLUME ONLY", "duration_seconds": 15.0},
                },
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    # Journal for the skeleton path: an override, a clear, a source that
    # contradicts the script hint (warns), and an orphan key.
    (cfg_dir / "sfx_S04E01_edits.jsonl").write_text(
        '{"ts": "2026-01-01T00:00:00+00:00", "key": "SFX: KEPT SOURCE", "fields": {"source": "SFX/from-journal.mp3"}}\n'
        '{"ts": "2026-01-01T00:00:01+00:00", "key": "MUSIC: VOLUME ONLY", "fields": {"volume_percentage": 77, "ramp_in_seconds": null}}\n'
        '{"ts": "2026-01-01T00:00:02+00:00", "key": "SFX: RENAMED AWAY", "fields": {"play_duration": 12}}\n'
        '{"ts": "2026-01-01T00:00:03+00:00", "scope": "defaults", "fields": {"music_volume_percentage": 44}}\n'
        "\n"
        "{not json}\n",
        encoding="utf-8",
    )

    # Parsed JSONs, straight from the Python parser, so read-only commands
    # (episode-summary, parsed-csv, status...) have real input to chew on.
    env = dict(os.environ, XIL_PROJECTROOT=str(WORKSPACE_FIXTURE))
    env.pop("ELEVENLABS_API_KEY", None)
    for md in sorted(scripts.glob("*.md")):
        r = _run([str(PY_XIL), "parse", str(md.relative_to(WORKSPACE_FIXTURE)), "--quiet"], cwd=WORKSPACE_FIXTURE, env=env)
        if r.returncode != 0:
            print(f"parse failed for {md.name}:\n{r.stdout}{r.stderr}", file=sys.stderr)
    # One episode parsed with --debug so csv-join has a parsed CSV plus the
    # skeleton cast/sfx configs it joins against.
    r = _run([str(PY_XIL), "parse", "scripts/Tech_Deep_Dive_S01E04.md", "--episode", "S01E04", "--debug", "--quiet"],
             cwd=WORKSPACE_FIXTURE, env=env)
    if r.returncode != 0:
        print(f"debug parse failed:\n{r.stdout}{r.stderr}", file=sys.stderr)
    shutil.rmtree(WORKSPACE_FIXTURE / "logs", ignore_errors=True)
    # sfx-hydrate --force: the KEPT SOURCE hint resolves on disk, so it is
    # replaced; the FILLED SOURCE hint does not, so that add still happens
    # but a forced replace elsewhere would be skipped.
    (WORKSPACE_FIXTURE / "SFX" / "hintshow").mkdir(parents=True, exist_ok=True)
    (WORKSPACE_FIXTURE / "SFX" / "hintshow" / "new.mp3").write_bytes(b"ID3\x03\x00new!")
    # sfx-restore on the S03E01 config: one override and one orphan.
    (cfg_dir / "sfx_S03E01_edits.jsonl").write_text(
        '{"ts": "2026-01-02T00:00:00+00:00", "key": "MUSIC: VOLUME ONLY", "fields": {"volume_percentage": 61}}\n'
        '{"ts": "2026-01-02T00:00:01+00:00", "key": "SFX: GONE", "fields": {"play_duration": 50}}\n',
        encoding="utf-8",
    )
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
    # A revised episode for `migrate`, `cleanup` and `splice`: an old and a
    # new parsed JSON differing by an inserted line, a speaker swap, a
    # punctuation-only edit and a deletion, plus stems on disk for some of
    # them so every migration status appears.
    def _entry(seq, kind, text, speaker=None, section="act1", scene=None):
        return {"seq": seq, "type": kind, "section": section, "scene": scene, "speaker": speaker,
                "direction": None, "text": text,
                "direction_type": "SFX" if kind == "direction" else None,
                "sfx_source": None, "sfx_overrides": None}

    old_entries = [
        _entry(1, "section_header", "ACT ONE"),
        _entry(2, "dialogue", "Kept line.", "adam"),
        _entry(3, "dialogue", "Vanishes from disk.", "adam"),
        _entry(4, "dialogue", "Reassigned line.", "adam"),
        _entry(5, "dialogue", "Punctuation \u2014 edited.", "maya"),
        _entry(6, "direction", "SFX: DOOR"),
        _entry(7, "dialogue", "Deleted later.", "maya"),
    ]
    new_entries = [
        _entry(1, "section_header", "ACT ONE"),
        _entry(2, "dialogue", "Kept line.", "adam"),
        _entry(3, "dialogue", "Vanishes from disk.", "adam"),
        _entry(4, "dialogue", "Reassigned line.", "maya"),
        _entry(5, "dialogue", "Punctuation - edited.", "maya"),
        _entry(6, "direction", "SFX: DOOR"),
        _entry(7, "dialogue", "Brand new line.", "adam"),
    ]

    def _parsed_doc(entries):
        dialogue = [e for e in entries if e["type"] == "dialogue"]
        return {
            "show": "Revised Show", "season": 2, "episode": 1, "title": "Revised",
            "season_title": None, "source_file": "revised_S02E01.md",
            "entries": entries,
            "stats": {
                "total_entries": len(entries),
                "dialogue_lines": len(dialogue),
                "direction_lines": sum(1 for e in entries if e["type"] == "direction"),
                "characters_for_tts": sum(len(e["text"]) for e in dialogue),
                "speakers": sorted({e["speaker"] for e in dialogue}),
                "sections": sorted({e["section"] for e in entries if e["section"]}),
            },
        }

    pdir = WORKSPACE_FIXTURE / "parsed" / "revisedshow"
    pdir.mkdir(parents=True, exist_ok=True)
    (pdir / "parsed_S02E01.json").write_text(json.dumps(_parsed_doc(new_entries), indent=2) + "\n", encoding="utf-8")
    (WORKSPACE_FIXTURE / "parsed" / "orig_parsed_revisedshow_S02E01.json").write_text(
        json.dumps(_parsed_doc(old_entries), indent=2) + "\n", encoding="utf-8"
    )
    rcfg = WORKSPACE_FIXTURE / "configs" / "revisedshow"
    rcfg.mkdir(parents=True, exist_ok=True)
    (rcfg / "project.json").write_text(json.dumps({"show": "Revised Show"}, indent=2) + "\n", encoding="utf-8")
    sdir = WORKSPACE_FIXTURE / "stems" / "revisedshow" / "S02E01"
    sdir.mkdir(parents=True, exist_ok=True)
    # 003's stem is deliberately absent so migrate reports MISSING; the last
    # three are a stale duplicate, an orphan seq and a header seq for cleanup.
    for _name in ("002_act1_adam.mp3", "004_act1_adam.mp3", "005_act1_maya.mp3", "006_act1_sfx.mp3",
                  "004_act1_maya.mp3", "099_act1_adam.mp3", "001_act1_sfx.mp3"):
        (sdir / _name).write_bytes(b"stem " + _name.encode())

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
    # Real, decodable audio for db-profile — generated by ffmpeg from its
    # own signal generators, so the bytes are reproducible and nothing
    # binary is committed. A silent file is included on purpose: it
    # profiles as -inf, which json.dumps writes as a bare -Infinity.
    audio = ws / "audio"
    (audio / "nested").mkdir(parents=True, exist_ok=True)
    tones = [
        ("audio/loud_sine.mp3", "sine=frequency=440:duration=2:sample_rate=44100", ["-ac", "2"]),
        ("audio/quiet_sine.mp3", "sine=frequency=220:duration=1:sample_rate=22050", ["-ac", "1", "-af", "volume=0.05"]),
        ("audio/silence.mp3", "anullsrc=r=8000:cl=mono:d=1", []),
        ("audio/nested/noise.mp3", "anoisesrc=d=1:c=pink:r=16000:a=0.3:seed=7", ["-ac", "1"]),
        ("audio/Mixed Case.MP3", "sine=frequency=880:duration=1:sample_rate=32000", ["-ac", "1"]),
    ]
    for rel, filt, extra in tones:
        out = ws / rel
        r = _run(["ffmpeg", "-v", "quiet", "-y", "-f", "lavfi", "-i", filt, *extra, str(out)])
        if r.returncode != 0:
            print(f"ffmpeg failed for {rel}:\n{r.stderr}", file=sys.stderr)
    (audio / "notes.txt").write_text("not audio\n")

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

    # A tagged, decodable SFX library for sfx-lib, sfx-impact and sfx-match.
    # Titles go in TIT2 and prompts in USLT through mutagen, exactly as
    # tag_mp3 writes them. Copies of one asset share a title and duration so
    # sfx-match's dedupe has something to collapse.
    library = [
        # (path, seconds, title, prompt)
        ("SFX/impactshow/bed_8s.mp3", 8, "MUSIC: LONG BED", "a long calm music bed"),
        ("SFX/impactshow/sting.mp3", 1, "SFX: STING", ""),
        ("SFX/matchshow/sfx_door-creaks-open.mp3", 1, "SFX: DOOR CREAKS OPEN", "an old wooden door creaking"),
        ("SFX/sfx_door-creaks-open.mp3", 1, "SFX: DOOR CREAKS OPEN", "an old wooden door creaking"),
        ("SFX/matchshow/AMBRain-heavy_rain_on_window.mp3", 2, "AMBIENCE: HEAVY RAIN ON A WINDOW", "heavy rain against glass"),
        ("SFX/sfx_phone-buzz.mp3", 1, "SFX: PHONE BUZZ", ""),
        ("SFX/sfx_phone-buzz-twice.mp3", 2, "SFX: PHONE BUZZ TWICE", ""),
        ("SFX/matchshow/MUSFolk-theme.mp3", 3, "", ""),
    ]
    for rel, secs, title, prompt in library:
        out = ws / rel
        out.parent.mkdir(parents=True, exist_ok=True)
        r = _run(["ffmpeg", "-v", "quiet", "-y", "-f", "lavfi", "-i",
                  f"sine=frequency=330:duration={secs}:sample_rate=44100", "-ac", "1", str(out)])
        if r.returncode != 0:
            print(f"ffmpeg failed for {rel}:\n{r.stderr}", file=sys.stderr)
        if title or prompt:
            r = _run([str(py), "-c",
                      "import sys; from mutagen.id3 import ID3, TIT2, TPE1, USLT; t = ID3(sys.argv[1]); "
                      "t.delall('TIT2'); t.delall('TPE1'); "
                      "sys.argv[2] and t.add(TIT2(encoding=3, text=sys.argv[2])); "
                      "t.add(TPE1(encoding=3, text='xil')); "
                      "sys.argv[3] and t.add(USLT(encoding=3, lang='eng', desc='', text=sys.argv[3])); t.save()",
                      str(out), title, prompt])
            if r.returncode != 0:
                print(f"tagging failed for {rel}:\n{r.stderr}", file=sys.stderr)

    def _sfx_config(slug, tag, effects):
        d = ws / "configs" / slug
        d.mkdir(parents=True, exist_ok=True)
        (d / f"sfx_{tag}.json").write_text(
            json.dumps({"show": slug, "defaults": {}, "effects": effects}, indent=2, ensure_ascii=False) + "\n",
            encoding="utf-8")

    # Every tier and every precedence rule sfx-impact knows.
    _sfx_config("impactshow", "S01E01", {
        "MUSIC: LONG BED": {"source": "SFX/impactshow/bed_8s.mp3", "duration_seconds": 5.0},
        "SFX: SHORT CLIP": {"source": "SFX/impactshow/bed_8s.mp3", "duration_seconds": 7.0},
        "SFX: STING": {"source": "SFX/impactshow/sting.mp3", "duration_seconds": 5.0},
        "AMBIENCE: LOOPED": {"source": "SFX/impactshow/bed_8s.mp3", "duration_seconds": 5.0, "loop": True},
        "SFX: HALF": {"source": "SFX/impactshow/bed_8s.mp3", "play_duration": 50},
        "SFX: FULL": {"source": "SFX/impactshow/sting.mp3", "duration_seconds": 0},
        "SFX: FAKE": {"source": "SFX/beat.mp3", "duration_seconds": 5.0},
        "SFX: GONE": {"source": "SFX/impactshow/nope.mp3", "duration_seconds": 5.0},
        "BEAT": {"type": "silence", "duration_seconds": 1.0},
        "SFX: NOT A DICT": "oops",
    })
    # sfx-match: an EXACT slug hit, a STRONG match, a REVIEW tie, a NONE, a
    # placeholder source, and a source that resolves (so it is skipped).
    _sfx_config("matchshow", "S01E01", {
        "SFX: DOOR CREAKS OPEN": {"source": "SFX/matchshow/old-door.mp3"},
        "AMBIENCE: RAIN ON WINDOW": {"source": "SFX/matchshow/rain.mp3", "loop": True},
        "SFX: PHONE BUZZ LOUD": {"source": "SFX/matchshow/buzz.mp3"},
        "SFX: GLASS SHATTERS": {"source": "NEW STEM NEEDED: sfx_glass-shatters.mp3"},
        "MUSIC: FOLK THEME": {"source": "SFX/matchshow/folk.mp3"},
        "SFX: STING": {"source": "SFX/impactshow/sting.mp3"},
        "BEAT": {"type": "silence", "duration_seconds": 1.0},
    })
    _sfx_config("matchshow", "S01E02", {
        "SFX: PHONE BUZZ": {"source": "SFX/phone.mp3"},
    })
    # sfx-lib --export-kit copies the scriptwriter doc from ./docs when the
    # package copy is absent, which it is for both implementations.
    (ws / "docs").mkdir(exist_ok=True)
    (ws / "docs" / "claude-scriptwriter-reference.md").write_text("# Scriptwriter reference (fixture)\n", encoding="utf-8")
    _record_mix(ws)
    _write_manifest()
    return 0


def _write_manifest() -> None:
    manifest = {
        "python_git_sha": _git_sha(CODEROOT),
        "python_version": _run([str(PY_XIL), "--version"]).stdout.strip(),
        "files": sorted(str(p.relative_to(WORKSPACE_FIXTURE)) for p in WORKSPACE_FIXTURE.rglob("*") if p.is_file()),
    }
    (FIXTURES / "MANIFEST.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"recorded {len(manifest['files'])} files into {WORKSPACE_FIXTURE}")


def cmd_record_mix(_: argparse.Namespace) -> int:
    """(Re)write only the mixing fixtures, leaving every other fixture as recorded."""
    _record_mix(WORKSPACE_FIXTURE)
    _write_manifest()
    return 0


def _record_mix(ws: Path) -> None:
    """A small episode that drives every path through daw, assemble and master.

    Stems are short ffmpeg tones at deliberately mixed rates and channel
    counts, so every layer build crosses pydub's sync/resample paths. The
    parsed script opens and closes each span type (a scoped PHONE FILTER
    included), loops ambience up to a STOP marker, carries preamble and
    postamble music, and leaves stale, duplicate and wrong-speaker stems on
    disk for collect_stem_plans to reject.
    """
    for rel in ("configs/mixshow", "parsed/mixshow", "stems/mixshow", "SFX/mixshow", "daw/mixshow"):
        shutil.rmtree(ws / rel, ignore_errors=True)
    py = PY_XIL.parent / "python"
    cfg = ws / "configs" / "mixshow"
    cfg.mkdir(parents=True)
    (cfg / "project.json").write_text(json.dumps({"show": "Mix Show"}, indent=2) + "\n", encoding="utf-8")

    def member(name, pan, flt):
        return {"full_name": name, "voice_id": "TBD", "pan": pan, "filter": flt, "role": "fixture"}

    cast = {
        "show": "Mix Show", "season": 1, "episode": 1, "title": "Every Layer", "season_title": "The Mix",
        "cast": {
            "host": member("Host", -0.3, None),
            "guest": member("Guest", 0.4, "phone"),
            "caller": member("Caller", 0.0, "speakerphone"),
            "old": member("Old Timer", 0.0, "vintage"),
            "dez": member("Dez", 0.2, False),
            "bot": member("Bot", 1.0, "Robot, vintage"),
        },
    }
    (cfg / "cast_S01E01.json").write_text(json.dumps(cast, indent=2) + "\n", encoding="utf-8")
    seq_cast = dict(cast, episode=2, title=None)
    (cfg / "cast_S01E02.json").write_text(json.dumps(seq_cast, indent=2) + "\n", encoding="utf-8")
    (cfg / "cast_S01E03.json").write_text(json.dumps(dict(cast, episode=3, title="Mastered"), indent=2) + "\n", encoding="utf-8")

    sfx = {
        "show": "Mix Show", "season": 1, "episode": 1,
        "defaults": {"music_volume_percentage": 80, "ambience_ramp_in_seconds": 0.5,
                     "sfx_volume_percentage": 90.0, "ramp_out_seconds": 0.2},
        "effects": {
            "MUSIC: INTRO THEME": {"source": "SFX/mixshow/theme.mp3", "play_duration": 60, "ramp_out_seconds": 0.3},
            "AMBIENCE: DINER": {"source": "SFX/mixshow/diner.mp3", "loop": True, "volume_percentage": 70},
            "AMBIENCE: RAIN": {"source": "SFX/mixshow/rain.mp3", "duration_seconds": 0.4},
            "SFX: DOOR \u2014 SLAM": {"source": "SFX/mixshow/door.mp3", "duration_seconds": 0},
            "SFX: PHONE BUZZ": {"source": "SFX/mixshow/buzz.mp3", "duration_seconds": 0.3},
            "BEAT": {"type": "silence", "duration_seconds": 0.5},
            "VINTAGE FILTER ENGAGES": {"source": "SFX/mixshow/crackle.mp3", "loop": True},
            "MUSIC: OUTRO": {"source": "SFX/mixshow/outro.mp3", "ramp_in_seconds": 0.25},
        },
        "vintage_scenes": ["scene-9"],
    }
    (cfg / "sfx_S01E01.json").write_text(json.dumps(sfx, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

    rows = [
        # seq, type, section, scene, speaker, direction_type, text
        (1, "section_header", "preamble", None, None, None, "PREAMBLE"),
        (2, "direction", "preamble", None, None, "MUSIC", "MUSIC: INTRO THEME"),
        (3, "dialogue", "preamble", None, "host", None, "Welcome to the mix, where every layer lines up."),
        (4, "section_header", "act1", None, None, None, "ACT ONE"),
        (5, "scene_header", "act1", "scene-1", None, None, "SCENE 1: DINER"),
        (6, "direction", "act1", "scene-1", None, "AMBIENCE", "AMBIENCE: DINER"),
        (7, "dialogue", "act1", "scene-1", "guest", None, "Hello from the phone line."),
        (8, "direction", "act1", "scene-1", None, "SFX", "SFX: DOOR - SLAM"),
        (9, "direction", "act1", "scene-1", None, "MUSIC", "MUSIC: STING"),
        (10, "dialogue", "act1", "scene-1", "host", None, "That door again."),
        (11, "direction", "act1", "scene-1", None, "BEAT", "BEAT"),
        (12, "direction", "act1", "scene-1", None, "AMBIENCE", "AMBIENCE: STOP"),
        (13, "direction", "act1", "scene-1", None, "SPEAKERPHONE", "SPEAKERPHONE: ENGAGES"),
        (14, "dialogue", "act1", "scene-1", "caller", None, "Can everyone hear me?"),
        (15, "direction", "act1", "scene-1", None, "SPEAKERPHONE", "SPEAKERPHONE: DISENGAGES"),
        (16, "scene_header", "act1", "scene-2", None, None, "SCENE 2: STREET"),
        (17, "direction", "act1", "scene-2", None, "AMBIENCE", "AMBIENCE: RAIN"),
        (18, "direction", "act1", "scene-2", None, "PHONE FILTER", "PHONE FILTER: ENGAGES DEZ"),
        (19, "dialogue", "act1", "scene-2", "dez", None, "It is raining here."),
        (20, "dialogue", "act1", "scene-2", "guest", None, "Here too."),
        (21, "direction", "act1", "scene-2", None, "PHONE FILTER", "PHONE FILTER: DISENGAGES"),
        (22, "direction", "act1", "scene-2", None, "VINTAGE FILTER", "VINTAGE FILTER ENGAGES"),
        (23, "dialogue", "act1", "scene-2", "old", None, "Back in my day."),
        (24, "dialogue", "act1", "scene-2", "bot", None, "Beep."),
        (25, "direction", "act1", "scene-2", None, "VINTAGE FILTER", "VINTAGE FILTER DISENGAGES"),
        (26, "direction", "act1", "scene-2", None, "SFX", "SFX: PHONE BUZZ"),
        (27, "dialogue", "postamble", None, "host", None, "Thanks for listening."),
        (28, "direction", "postamble", None, None, "MUSIC", "MUSIC: OUTRO"),
    ]
    entries = [{"seq": q, "type": t, "section": sec, "scene": sc, "speaker": sp, "direction": None,
                "text": txt, "direction_type": dt, "sfx_source": None, "sfx_overrides": None}
               for q, t, sec, sc, sp, dt, txt in rows]
    pdir = ws / "parsed" / "mixshow"
    pdir.mkdir(parents=True)
    (pdir / "parsed_S01E01.json").write_text(
        json.dumps({"show": "Mix Show", "season": 1, "episode": 1, "title": "Every Layer", "entries": entries}, indent=2) + "\n",
        encoding="utf-8")

    def tone(out: Path, filt: str, extra: list[str]) -> None:
        out.parent.mkdir(parents=True, exist_ok=True)
        r = _run(["ffmpeg", "-v", "quiet", "-y", "-f", "lavfi", "-i", filt, *extra, str(out)])
        if r.returncode != 0:
            print(f"ffmpeg failed for {out}:\n{r.stderr}", file=sys.stderr)

    stems = ws / "stems" / "mixshow" / "S01E01"
    sine = "sine=frequency={f}:duration={d}:sample_rate={r}"
    plan = {
        "002_preamble_sfx.mp3": (sine.format(f=330, d=1.5, r=44100), ["-ac", "2"]),
        "003_preamble_host.mp3": (sine.format(f=220, d=0.9, r=24000), ["-ac", "1", "-af", "volume=2.5"]),
        "006_act1-scene-1_sfx.mp3": (sine.format(f=110, d=0.35, r=22050), ["-ac", "1"]),
        "007_act1-scene-1_guest.mp3": (sine.format(f=440, d=0.8, r=44100), ["-ac", "2"]),
        "007_act1-scene-1_host.mp3": (sine.format(f=445, d=0.3, r=44100), ["-ac", "1"]),
        "008_act1-scene-1_sfx.mp3": (sine.format(f=880, d=0.4, r=48000), ["-ac", "1", "-af", "volume=3"]),
        "008_act1_sfx.mp3": (sine.format(f=890, d=0.4, r=48000), ["-ac", "1"]),
        "009_act1-scene-1_sfx.mp3": (sine.format(f=660, d=0.5, r=44100), ["-ac", "1"]),
        "010_act1-scene-1_host.mp3": (sine.format(f=250, d=0.7, r=24000), ["-ac", "1"]),
        "011_act1-scene-1_sfx.mp3": ("anullsrc=r=44100:cl=mono:d=0.5", []),
        "014_act1-scene-1_caller.mp3": (sine.format(f=500, d=0.6, r=22050), ["-ac", "1"]),
        "017_act1-scene-2_sfx.mp3": ("anoisesrc=d=0.8:c=brown:r=11025:a=0.4:seed=5", ["-ac", "1"]),
        "019_act1-scene-2_dez.mp3": (sine.format(f=300, d=0.6, r=16000), ["-ac", "1"]),
        "020_act1-scene-2_guest.mp3": (sine.format(f=350, d=0.5, r=44100), ["-ac", "2"]),
        "022_act1-scene-2_sfx.mp3": ("anoisesrc=d=0.3:c=white:r=16000:a=0.2:seed=3", ["-ac", "1"]),
        "023_act1-scene-2_old.mp3": (sine.format(f=180, d=0.7, r=44100), ["-ac", "1"]),
        "024_act1-scene-2_bot.mp3": (sine.format(f=900, d=0.4, r=32000), ["-ac", "1"]),
        "026_act1-scene-2_sfx.mp3": (sine.format(f=700, d=0.9, r=8000), ["-ac", "1"]),
        "027_postamble_host.mp3": (sine.format(f=210, d=0.5, r=24000), ["-ac", "1"]),
        "028_postamble_sfx.mp3": (sine.format(f=520, d=1.0, r=48000), ["-ac", "2"]),
        "099_act1_host.mp3": (sine.format(f=100, d=0.2, r=8000), ["-ac", "1"]),
        "005_act1-scene-1_sfx.mp3": (sine.format(f=100, d=0.2, r=8000), ["-ac", "1"]),
        "preamble_intro.mp3": (sine.format(f=100, d=0.2, r=8000), ["-ac", "1"]),
    }
    for name, (filt, extra) in plan.items():
        tone(stems / name, filt, extra)
    (stems / "notes.txt").write_text("not a stem\n")
    # A TTS model note in COMM, as the producer writes it, for the timeline tooltip.
    r = _run([str(py), "-c",
              "import sys; from mutagen.id3 import ID3, COMM; t = ID3(sys.argv[1]); "
              "t.add(COMM(encoding=3, lang='eng', desc='', text='eleven_v3')); t.save()",
              str(stems / "003_preamble_host.mp3")])
    if r.returncode != 0:
        print(f"COMM tagging failed:\n{r.stderr}", file=sys.stderr)

    # S01E02: stems but no parsed script — assemble's sequential fallback.
    seq_stems = ws / "stems" / "mixshow" / "S01E02"
    tone(seq_stems / "001_act1_host.mp3", sine.format(f=260, d=0.4, r=22050), ["-ac", "1"])
    tone(seq_stems / "002_act1_guest.mp3", sine.format(f=390, d=0.3, r=44100), ["-ac", "2"])
    tone(seq_stems / "003_act1_sfx.mp3", sine.format(f=990, d=0.2, r=48000), ["-ac", "1"])

    # S01E03: small layer WAVs of mixed formats for master, plus cover art.
    daw3 = ws / "daw" / "mixshow" / "S01E03"
    tone(daw3 / "S01E03_layer_dialogue.wav", sine.format(f=300, d=2.0, r=44100), ["-ac", "2", "-c:a", "pcm_s16le"])
    tone(daw3 / "S01E03_layer_music.wav", sine.format(f=500, d=2.0, r=22050), ["-ac", "1", "-c:a", "pcm_s16le"])
    tone(daw3 / "S01E03_layer_vintage_filter.wav", "anullsrc=r=11025:cl=mono:d=2", ["-c:a", "pcm_s16le"])
    tone(cfg / "cover_art.png", "color=c=teal:s=16x16:d=1", ["-frames:v", "1"])


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
    # `xil assemble` plays its master through mpg123 when it is done. A stub
    # keeps a parity run silent and the same on machines with and without it.
    stub = SCRATCH / "bin"
    stub.mkdir(parents=True, exist_ok=True)
    (stub / "mpg123").write_text("#!/bin/sh\nexit 0\n")
    (stub / "mpg123").chmod(0o755)
    env["PATH"] = f"{stub}{os.pathsep}{env.get('PATH', '')}"
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


def _id3_frames(p: Path) -> dict[str, list[str]] | None:
    """Every ID3 frame as text, keyed by mutagen's HashKey; None without mutagen.

    Compared as content, not bytes: mutagen and the Rust id3 crate pad and
    order a tag differently, and neither is part of what a player reads.
    """
    try:
        from mutagen.id3 import ID3, ID3NoHeaderError
        from mutagen.wave import WAVE
    except ImportError:
        return None
    try:
        tags = WAVE(p).tags if p.suffix.lower() == ".wav" else ID3(p)
    except (ID3NoHeaderError, Exception):
        return {}
    if tags is None:
        return {}
    out = {}
    for key, frame in tags.items():
        if hasattr(frame, "data"):
            out[key] = [getattr(frame, "mime", ""), str(len(frame.data)), __import__("hashlib").sha256(frame.data).hexdigest()]
        else:
            out[key] = [str(t) for t in getattr(frame, "text", [str(frame)])]
    return out


def _compare_tags(a: Path, b: Path) -> str | None:
    fa, fb = _id3_frames(a), _id3_frames(b)
    if fa is None or fa == fb:
        return None
    keys = sorted(set(fa) | set(fb))
    diff = [k for k in keys if fa.get(k) != fb.get(k)]
    return "ID3 frames differ: " + ", ".join(f"{k} py={fa.get(k)} rs={fb.get(k)}" for k in diff[:4])


def _compare_wav(a: Path, b: Path) -> str | None:
    def pcm(p: Path) -> tuple[bytes, bytes]:
        """(fmt chunk, data chunk) by declared size — the tag chunk after them is compared separately."""
        data = p.read_bytes()
        fmt = b""
        pos = 12
        while pos + 8 <= len(data):
            cid, size = data[pos:pos + 4], int.from_bytes(data[pos + 4:pos + 8], "little")
            if cid == b"fmt ":
                fmt = data[pos + 8:pos + 8 + size]
            if cid == b"data":
                return fmt, data[pos + 8:pos + 8 + size]
            pos += 8 + size + (size & 1)
        return fmt, data

    fa, da = pcm(a)
    fb, db = pcm(b)
    if fa != fb:
        return "WAV format differs"
    if da != db:
        if len(da) != len(db):
            return f"PCM payload differs: {len(da)} vs {len(db)} bytes"
        first = next(i for i in range(len(da)) if da[i] != db[i])
        return f"PCM payload differs from byte {first} of {len(da)}"
    return _compare_tags(a, b)


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
    return _compare_tags(a, b)


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


def cmd_sweep(ns: argparse.Namespace) -> int:
    """Parse every real production script under both implementations and diff.

    The fixture scripts are a handful of tidy samples; the shows in
    $XIL_PROJECTROOT are years of real authoring with every oddity the
    parser ever had to absorb. This reads them and writes nothing back.
    """
    src_root = Path(ns.scripts or (Path(os.environ["XIL_PROJECTROOT"]) / "scripts"))
    if not src_root.is_dir():
        print(f"no scripts directory at {src_root}", file=sys.stderr)
        return 2
    scripts = sorted(p for p in src_root.rglob("*.md") if p.is_file())
    if ns.limit:
        scripts = scripts[: ns.limit]
    if not scripts:
        print(f"no .md scripts under {src_root}", file=sys.stderr)
        return 2

    _native_commands()  # fail fast on a stale binary
    print(f"sweeping `xil {ns.command}` over {len(scripts)} script(s) from {src_root}")
    failed = 0
    for script in scripts:
        rel = script.relative_to(src_root)
        results = {}
        for side, force_py in (("py", True), ("rs", False)):
            ws = SCRATCH / f"sweep-{side}"
            if ws.exists():
                shutil.rmtree(ws)
            (ws / "scripts").mkdir(parents=True)
            target = ws / "scripts" / script.name
            shutil.copy2(script, target)
            env = dict(os.environ, XIL_PROJECTROOT=str(ws), XIL_TRACE_IMPL="1")
            env.pop("ELEVENLABS_API_KEY", None)
            if force_py:
                env["XIL_FORCE_PY"] = "all"
            else:
                env.pop("XIL_FORCE_PY", None)
            proc = _run([str(RUST_XIL), *ns.command.split(), f"scripts/{script.name}", *ns.args], cwd=ws, env=env)
            out = sorted((ws / "parsed").rglob("*.json"))
            written = out[0].read_text(encoding="utf-8") if out else None
            results[side] = (proc, written, _impl_of(proc))

        py_proc, py_json, _ = results["py"]
        rs_proc, rs_json, rs_impl = results["rs"]
        problems = []
        if rs_impl != "native":
            problems.append(f"the Rust side ran: {rs_impl}")
        if py_proc.returncode != rs_proc.returncode:
            problems.append(f"exit code: py={py_proc.returncode} rs={rs_proc.returncode}")
        if py_json != rs_json:
            if py_json is None or rs_json is None:
                problems.append(f"output written by py={py_json is not None} rs={rs_json is not None}")
            else:
                problems.append("parsed JSON differs: " + _first_diff(py_json, rs_json))
        # The impl trace is on stderr and differs by design; drop it first.
        py_err, rs_err = norm_err(py_proc.stderr), norm_err(rs_proc.stderr)
        if py_err != rs_err:
            problems.append("stderr differs: " + _first_diff(py_err, rs_err))
        if norm_out(py_proc.stdout) != norm_out(rs_proc.stdout):
            problems.append("stdout differs: " + _first_diff(norm_out(py_proc.stdout), norm_out(rs_proc.stdout)))

        if problems:
            failed += 1
            print(f"[FAIL] {rel}")
            for p in problems:
                print(f"        {p}")
        elif ns.verbose:
            print(f"[PASS] {rel}")
    print(f"{len(scripts) - failed}/{len(scripts)} scripts byte-identical")
    return 1 if failed else 0


def norm_out(text: str) -> str:
    for side in ("py", "rs"):
        text = text.replace(str(SCRATCH / f"sweep-{side}"), "<WS>")
    return _mask(text)


def norm_err(text: str) -> str:
    """stderr, minus the implementation trace the harness itself asked for."""
    return norm_out("\n".join(ln for ln in text.splitlines() if not ln.startswith("rxil-impl: ")))


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("record", help="seed fixtures/ from the Python repo").set_defaults(fn=cmd_record)
    sub.add_parser("record-mix", help="rewrite only the daw/assemble/master fixtures").set_defaults(fn=cmd_record_mix)
    c = sub.add_parser("check", help="run parity checks")
    c.add_argument("names", nargs="*", help="check names from suite.toml")
    c.add_argument("--suite", action="store_true", help="run every check")
    c.set_defaults(fn=cmd_check)
    s = sub.add_parser("sweep", help="run one command over every real script under both implementations")
    s.add_argument("--command", default="parse", help="subcommand to sweep (default: parse)")
    s.add_argument("--args", nargs="*", default=["--quiet"], help="extra arguments after the script path")
    s.add_argument("--scripts", default=None, help="script root (default: $XIL_PROJECTROOT/scripts)")
    s.add_argument("--limit", type=int, default=0, help="stop after N scripts")
    s.add_argument("--verbose", "-v", action="store_true", help="print passing scripts too")
    s.set_defaults(fn=cmd_sweep)
    ns = p.parse_args()
    if ns.cmd == "check" and not ns.suite and not ns.names:
        p.error("give check names or --suite")
    return ns.fn(ns)


if __name__ == "__main__":
    sys.exit(main())
