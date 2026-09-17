# xil-pipeline

Show-agnostic audio production pipeline that turns a markdown script into a
podcast-ready MP3.

A `project.json` file sets the show name; every command derives file paths
from it via a shared slug. The pipeline parses scripts, generates voices and
SFX, assembles a rough master, exports isolated WAV layers for DAW mixing, and
produces a final master MP3. Supporting utilities handle voice discovery, SFX
generation, stem migration on script revisions, stale cleanup, and Studio
import/export. Every command that calls a paid API supports `--dry-run` to
preview costs before spending quota.

`xil` is a single Rust binary. The only Python left is the three optional ML
workers (Chatterbox Turbo, Whisper, MMAudio), each in its own venv.

## Installation

```bash
git clone https://github.com/xilcmd/rxil-pipeline
cd rxil-pipeline
cargo build --release            # the binary: target/release/xil
```

On WSL, clone into the Linux filesystem (for example `~/src/rust/`), not
under `/mnt/c`: builds and tests there are many times slower.

Requirements:

- `ffmpeg` on `PATH` — every decode and encode goes through it.
- `XIL_PROJECTROOT` — the workspace (scripts, configs, stems, SFX). Defaults to
  the current directory.
- `XIL_CODEROOT` — a checkout of
  [xil-pipeline](https://github.com/xilcmd/xil-pipeline), only for the ML
  workers: their scripts ship in its `src/xil_pipeline/`, and their venvs live
  beside it.

`xil status --toolchain` shows which worker scripts and venvs it found.

The `xil-<command>` names used in older notes still work when the binary is
linked under that name (`ln -s xil xil-parse`); `xil <command>` is the same.

### Optional: local GPU TTS (Chatterbox Turbo)

The default dialogue backend is the ElevenLabs API. For a free, local,
GPU-accelerated alternative with per-character voice cloning, set up a
dedicated `venv-chatterbox/` at your code root (`XIL_CODEROOT`). It hosts
**Chatterbox Turbo**, which natively renders 19 paralinguistic tags (emotion,
delivery style, and vocal gestures — see
[the pipeline reference](internals/pipeline.md#chatterbox-turbo-paralinguistic-tags)).

```bash
# Needs uv (https://docs.astral.sh/uv/): curl -LsSf https://astral.sh/uv/install.sh | sh

# First-time setup (CUDA 12.4 wheels shown; adjust for your GPU/driver)
uv venv venv-chatterbox
uv pip install --python venv-chatterbox/bin/python \
    'torch==2.6.0' 'torchaudio==2.6.0' \
    --index-url https://download.pytorch.org/whl/cu124
uv pip install --python venv-chatterbox/bin/python chatterbox-tts   # provides Chatterbox Turbo

# Model weights auto-download from Hugging Face on first run. If the Turbo repo
# (ResembleAI/chatterbox-turbo) is gated for your account, authenticate first:
export HF_TOKEN=hf_...                               # or: huggingface-cli login
```

Then place a per-character reference clip at `voice_refs/<speaker_key>.wav`
(Chatterbox Turbo requires clips **longer than 5 seconds**) and select the
backend:

```bash
xil produce --episode S01E01 --backend chatterbox-turbo
```

`--chatterbox-python PATH` overrides the auto-detected venv Python.
`--backend chatterbox` is a deprecated alias that warns and uses Turbo.

Under `chatterbox-turbo` you can write cues straight into dialogue —
`[angry]` `[fear]` `[surprised]` `[happy]` `[crying]` `[sarcastic]`
`[whispering]` `[dramatic]` `[narration]` `[advertisement]` `[laugh]`
`[chuckle]` `[sigh]` `[gasp]` `[groan]` `[cough]` `[sniff]` `[shush]`
`[clear throat]`. Spelling is exact (no plurals: `[laugh]`, not `[laughs]`);
any other bracketed tag is stripped, so ElevenLabs-only tags can safely stay in
a shared script. Lines longer than about 250 characters are rendered in
sentence-sized chunks, because Turbo caps each generation at 40 seconds.

### Optional: local SFX generation (MMAudio) — non-commercial only

`--sfx-backend mmaudio` generates sound effects locally instead of calling the
ElevenLabs API. It runs in a dedicated `venv-mmaudio/`, needs ~6 GB of VRAM,
and installs from a git clone rather than PyPI:

```bash
uv venv venv-mmaudio
git clone https://github.com/hkchengrex/MMAudio
uv pip install --python venv-mmaudio/bin/python -e MMAudio

# MMAudio declares `torch >= 2.5.1` with no upper bound, so the line above pulls
# the newest torch — currently a CUDA 13 build. On a CUDA 12.x driver that
# silently falls back to CPU ("NVIDIA driver on your system is too old") and
# breaks torchaudio. Re-pin a driver-matched stack AFTERWARDS, not before:
uv pip install --python venv-mmaudio/bin/python \
    'torch==2.6.0' 'torchaudio==2.6.0' 'torchvision==0.21.0' \
    --index-url https://download.pytorch.org/whl/cu124

# Confirm the GPU is actually visible before generating anything:
venv-mmaudio/bin/python -c "import torch; print(torch.cuda.is_available())"   # must print True
```

> **⚠️ MMAudio's model weights are CC BY-NC 4.0 — non-commercial use only.**
> The code is MIT licensed; the checkpoints are not. Audio generated here must
> not appear in a monetised production. `--mmaudio-accept-noncommercial` is
> required or the backend refuses to start, and every generated asset is tagged
> `.mmaudio` in its filename and carries the licence notice in its ID3 comment
> so it stays identifiable later.

```bash
xil sfx --episode S01E01 --gen-sfx --sfx-backend mmaudio --mmaudio-accept-noncommercial
```

MMAudio is trained at 8 seconds, so cues are generated at that length and
trimmed to each cue's `duration_seconds` — this produces better audio than
asking the model for a short clip directly.

## Quick Start

See [the Tech Deep Dive sample](samples/Tech_Deep_Dive_S01E04.md) for an
example of the markdown script format the pipeline expects. It demonstrates
dialogue, acting directions, SFX/ambience/music cues, beats, sections, and
scenes.

```bash
# Scaffold a new project workspace (creates a copy of the sample script)
xil init my-show --show "My Podcast"
cd my-show

# Scan the sample script (pre-flight check)
# Always run scan before parse when onboarding a new episode — it will catch
# unrecognized speakers before they silently disappear from the parsed output.
xil scan scripts/sample_S01E01.md --speakers configs/my-show/speakers.json

# Parse into structured JSON
xil parse scripts/sample_S01E01.md --episode S01E01 --speakers configs/my-show/speakers.json

# Preview TTS character cost (no API calls)
xil produce --episode S01E01 --dry-run

# Generate voice and SFX stems (requires ELEVENLABS_API_KEY — see Environment below)
xil produce --episode S01E01

# Export DAW layers for mixing in Audacity
xil daw --episode S01E01

# Produce final master MP3
xil master --episode S01E01
```

`xil gui` opens the same workflow as a web dashboard at
<http://localhost:7860>. It binds `127.0.0.1`; to reach it from another
machine, forward the port (`ssh -L 7860:127.0.0.1:7860 <host>`).

## Commands

`xil --help` lists every command in pipeline order with its `XILP`/`XILU`
reference number; `xil <command> --help` (or `man xil-<command>`) shows its
options. The [pipeline reference](internals/pipeline.md) documents each stage
in depth.

## Configuration

- **`project.json`** — show name (derives all file paths via slug)
- **`speakers.json`** — speaker names the parser recognizes (optional, built-in defaults for sample)
- **`cast_<TAG>.json`** — voice assignments, speaker settings
- **`sfx_<TAG>.json`** — sound effect mappings and API parameters

All commands accept `--show` to override the show name. Resolution order:
`--show` flag > `project.json` > default `"sample"`.

### Episode tag formats

The `<TAG>` portion of all file and directory names supports any string. Use
`--episode` for standard episodic content or `--tag` for non-episodic formats:

| Content type | Tag format | Examples | Notes |
|---|---|---|---|
| Podcast episode | `S01E04` | `S02E11`, `S03E01` | Default — derived from script header |
| Audiobook chapter | `V01C03` | `V01C01`–`V01C20` | Volume + Chapter; use `--tag V01C03` with `xil parse` |
| Drama short | `S01D01` | `S01D03` | Season + Drama number; or just use `S01E01` |
| Standalone one-shot | `E01` | `E01`–`E99` | No season prefix; standard `--episode E01` works |
| Bonus / special | `BONUS01` | `TRAILER`, `BONUS02` | Any string via `--tag`; use uppercase by convention |

Episodic tags (`S01E04`, `E01`) are derived automatically from the script
header. All other formats require `--tag` on `xil parse`:

```bash
xil parse scripts/gatsby_V01C03.md --tag V01C03
xil produce --episode V01C03 --dry-run
xil daw --episode V01C03
```

Stems are stored under `stems/<slug>/<TAG>/`, so multiple shows and tag
formats coexist safely in one workspace.

See the [SFX Reuse Guide](guides/sfx-reuse-guide.md) for workflows that
minimize ElevenLabs API credit usage by referencing existing assets in the
`SFX/` library.

## Environment

### API keys

| Commands | Needs |
|----------|-------|
| `xil produce`, `xil sfx`, `xil studio-onboard`, `xil sample`, `xil voices`, `xil cues --generate` | `ELEVENLABS_API_KEY` |
| `xil publish` | `ANTHROPIC_API_KEY` |
| All other commands (`xil scan`, `xil parse`, `xil daw`, `xil master`, …) | nothing |

**Obtain an ElevenLabs key:** <https://elevenlabs.io> → Profile → API Keys

```bash
export ELEVENLABS_API_KEY=your_key_here                        # this shell
echo 'export ELEVENLABS_API_KEY=your_key_here' >> ~/.bashrc    # persist
```

Always use `--dry-run` first to preview character cost before making API calls.

## Man pages

```bash
export MANPATH="/path/to/rxil-pipeline/man:$(manpath 2>/dev/null)"
man xil
man xil-produce
```

## License

AGPL-3.0.
