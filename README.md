# rxil-pipeline

Rust port of [xil-pipeline](https://github.com/xilcmd/xil-pipeline), the
show-agnostic audio production pipeline (markdown script → podcast MP3).

All 38 `xil` commands are implemented in Rust. The only Python left at run
time is the three ML workers (Chatterbox Turbo, Whisper, MMAudio), each in
its own venv; `xil` talks to them over JSON lines.

The port was checked against that repository's **main branch**, not its PyPI
release: its version string has read `0.3.2` for many commits past the tag of
the same name. CI pins the exact reference commit.

## Layout

| Path | What |
| --- | --- |
| `crates/xil-core` | workspace paths, config models, edit journal, logging |
| `crates/xil-audio` | PCM math matching pydub/audioop, ffmpeg bridge, ID3/WAV tags |
| `crates/xil-api` | ElevenLabs / Anthropic / gTTS clients |
| `crates/xil-workers` | JSON-over-stdio client for the Python ML workers |
| `crates/xil-web` | `xil gui`: the axum + htmx dashboard |
| `crates/xil-cli` | the `xil` binary: command table and one module per stage |
| `man/man1` | man pages, generated from the clap definitions |
| `docs/book` | the user guide (mdBook) |
| `tools/parity` | Python-vs-Rust output comparison harness |

## Install

Prebuilt binaries for Linux, macOS and Windows are attached to each
[release](https://github.com/xilcmd/rxil-pipeline/releases). Unpack, put `xil`
on your `PATH`, and install `ffmpeg` — it is not bundled. To build instead:

## Build

```bash
cargo build                      # binaries land in target/
cargo nextest run --workspace    # unit tests
```

On WSL, keep the clone in the Linux filesystem (for example
`~/src/rust/rxil-pipeline`), not under `/mnt/c`. Every file the build and the
parity suite touch on drvfs is a round trip to Windows.

## Run

```bash
export XIL_PROJECTROOT=/path/to/workspace    # scripts, configs, stems, SFX
export XIL_CODEROOT=/path/to/xil-pipeline    # worker scripts and their venvs
target/release/xil --help
target/release/xil status --toolchain   # worker scripts and venvs found
```

`XIL_CODEROOT` is only needed by the stages that start an ML worker
(`produce`/`sample` with Chatterbox, `stem-verify`, `sfx`/`produce` with
MMAudio).

## Man pages and guide

```bash
xil --generate-man man/man1      # regenerate after changing any command's options
man -l man/man1/xil-parse.1
mdbook build docs/book           # writes docs/book/book/
mdbook serve docs/book           # read it at http://localhost:3000
```

CI fails when the committed man pages are out of date. Every push to `main`
publishes the guide to <https://xilcmd.github.io/rxil-pipeline/>.

## Parity

The harness runs each check with the Python `xil` and with the Rust `xil` on
twin copies of a fixture workspace, then diffs exit codes, output and every
file written. Python is a test-only dependency.

```bash
export XIL_PY_BIN=/path/to/xil-pipeline/venv/bin/xil
python tools/parity/parity.py check --suite    # run every check in suite.toml
python tools/parity/parity.py check parse-sample
```

Use a Python with numpy (the xil-pipeline venv): the audio comparisons need it.
