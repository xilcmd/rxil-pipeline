# rxil-pipeline

Rust port of [xil-pipeline](https://github.com/xilcmd/xil-pipeline), the
show-agnostic audio production pipeline (markdown script → podcast MP3).

The reference is that repository's **main branch**, not its PyPI release:
its version string has read `0.3.2` for many commits past the tag of the
same name. CI pins the exact reference commit.

The port is a **strangler**: the Rust `xil` binary ships from day one and
hands any command it does not yet implement to the Python package. A command
switches to Rust only when `tools/parity/` proves the output matches.

## Layout

| Path | What |
| --- | --- |
| `crates/xil-core` | workspace paths, config models, edit journal, logging |
| `crates/xil-audio` | PCM math matching pydub/audioop, ffmpeg bridge, ID3/WAV tags |
| `crates/xil-api` | ElevenLabs / Anthropic / gTTS clients |
| `crates/xil-workers` | JSON-over-stdio client for the Python ML workers |
| `crates/xil-cli` | the `xil` binary: command table, delegation, stages |
| `tools/parity` | Python-vs-Rust output comparison harness |

## Build

```bash
cargo build                      # target dir is pinned to ~/.cargo-target/rxil (ext4)
cargo nextest run --workspace    # unit tests
```

The source tree may live on `/mnt/c` (drvfs); object files must not.
`.cargo/config.toml` takes care of that.

## Run

```bash
export XIL_CODEROOT=/path/to/xil-pipeline   # where the Python venv lives
~/.cargo-target/rxil/debug/xil --help
~/.cargo-target/rxil/debug/xil status --toolchain   # shows which Python it delegates to
```

Environment knobs:

- `XIL_PY_BIN` — explicit path to the Python `xil` (beats `$XIL_CODEROOT/venv/bin/xil`).
- `XIL_FORCE_PY=cmd1,cmd2` or `all` — run those commands through Python even
  when a Rust implementation exists. The parity harness uses this.

## Parity

```bash
python3 tools/parity/parity.py record          # seed fixtures from the Python repo once
python3 tools/parity/parity.py check --suite   # run every check in suite.toml
python3 tools/parity/parity.py check parse-sample
```

## Status

All 38 commands delegate to Python. See `crates/xil-cli/src/commands.rs`.
