# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Arkeology Synth is a multitimbral MIDI synth engine for the terminal (Rust, ratatui TUI, cpal audio, midir MIDI): up to 16 synth slots, each on a MIDI channel, mixed through insert FX, a reverb send and a master bus. README.md has the user-facing feature list and key bindings.

## Commands

Rust comes from mise (`mise.toml`); if `cargo` isn't on PATH, it's in `~/.cargo/bin`. The compiler version is pinned in `rust-toolchain.toml` (and in the CI workflow): new Rust releases add clippy lints, and CI treats warnings as errors.

```sh
cargo build --release
cargo test --release                       # full suite (~80 tests, ~1 s in release)
cargo test --release <name>                # one test or module, e.g. `physical`, `fx::tests::eq`
cargo test <name> -- --nocapture           # see eprintln! output
cargo clippy --all-targets                 # must stay warning-free
cargo fmt                                  # default rustfmt settings; keep the tree formatted
cargo run --release -- --render-demo out.wav   # offline render, no audio device or TUI
cargo run --release -- --data-dir /tmp/x --render-patches /tmp/renders [--only organ]   # every factory patch to WAV + levels.tsv
cargo run --release -- --list-devices
SHOW_UI=1 cargo test renders_and_navigates -- --nocapture   # print TUI screens rendered by tests
```

mise tasks: `mise run run|test|demo|midi-send`. `[profile.dev]` uses opt-level 2 because unoptimised DSP can't run in real time.

The TUI can't be driven interactively from a tool call. To check UI changes, use the headless `TestBackend` helpers in `app.rs` tests (`test_app`, `screen`), or run the binary under a pty with an explicit window size and `--data-dir` pointing at a scratch directory (never the user's real data dir).

## Architecture

**Threads and real-time rules.** Three threads talk through lock-free queues (`crossbeam_queue::ArrayQueue`):
- The audio callback owns `engine::Engine`. It must never lock, allocate or free.
- The MIDI thread (midir callbacks) pushes `Command::MidiAt` (stamped with its arrival `Instant`) to the engine and a copy to the UI queue. `Engine::process_at` places stamped MIDI at the matching sample of the next buffer (offset = arrival − previous callback), rendering in sub-blocks that stop at each event; `Command::Midi` (unstamped, e.g. offline renders) applies at the buffer start.
- The UI thread owns `app::App`: the model, input handling and all persistence.

Anything heap-allocated (a whole `engine::Slot`, an `fx::FxUnit` with its delay lines, an `Arc<Sample>`) is built on the UI thread and moved in with a `Command`. Whatever the engine replaces goes back through the garbage queue (`engine::Garbage`) so the UI thread drops it. Meters, voice counts and CPU load come back via atomics in `engine::Telemetry`, as does the master output itself (`spectrum::Scope`, a ring of atomic samples) for the UI-side FFT analyzer in `spectrum.rs`. Keep this discipline in any new feature.

**Parameters are the backbone.** Every synth exposes a static table of `params::ParamDesc` (key, range, scale, unit, group). A slot's state is a flat `Vec<f32>` laid out as `synth::COMMON` (volume, pan, send, transpose, bend range), then the synth's own `PARAMS`, then the insert-FX block `fx::PARAMS` (3 units × `fx::STRIDE`). `SynthKind::param/param_count/fx_base` encapsulate this layout. The master bus follows the same pattern with `params::master::PARAMS` (base params + FX). The same tables drive:
- the engine: `SetParam` → `engine::ParamGlide` (continuous params glide over ~20 ms, in log space for `Scale::Exp`; others jump) → slot `dirty` → `Instrument::update(&params)` once per block while gliding, which recomputes cached coefficients. `update` must therefore stay cheap and side-effect free (it runs per block during a glide). The master bus glides the same way
- the generic TUI editor (groups = contiguous runs of `group`)
- patch/session JSON, which stores values **by key** so tables can grow
- MIDI learn and the MCP tools

Adding a parameter means adding a table entry and reading it in `update`. Visibility is computed, not stored: `SynthKind::param_visible` hides parameters of inactive FX types and of other physical-synth models. The UI, MCP `get_params` and patch saving all respect it.

**Synth engines** (`src/synth/`), dispatched statically through `synth::Instrument`:
- Polyphonic engines (`fm`, `analog`, `granular`, `sampler`, `physical`, `vocal`) implement `synth::Voice` and reuse `synth::Poly`, which handles allocation, stealing, sustain, glide and bend.
- Mono or one-shot engines (`acid`, `drums`, `kit`) handle notes themselves and implement the private `PolyControl` trait. `tonewheel` is in between: it uses `Poly` for voices but wraps it to track held keys (single-trigger percussion) and runs a shared post-chain (vibrato scanner, overdrive, rotary speaker).
- To add an engine: a `SynthKind` variant (label, long name, `ALL` order), a params table, an `Instrument` variant wired into every match in `synth/mod.rs`, a UI colour in `ui.rs`, factory patches in `patch.rs`, and the kind name in the MCP tool schema (`mcp.rs`) and in `app/tools.rs` errors.
- Large engine structs are boxed in `Instrument` (clippy's `large_enum_variant`).

**Shared DSP** lives in `dsp.rs`: sine table, ADSR, SVF, ZDF ladder, PolyBLEP, RBJ biquads. The reverb (Classic Freeverb, a modulated 8-line FDN for Room/Chamber/Hall/Cathedral/Plate, and a dispersive spring) is in `reverb.rs`; buffers are sized for the largest type so switching type never allocates, and per-type levels are matched on pink noise, and `sample.rs` handles WAV/FLAC loading plus load-time analysis (waveform overview, onsets for slicing, YIN pitch detection). Insert effects are in `fx.rs`; the guitar Pedal and Amp types' DSP (4× oversampled clipping, tone stack, cabinet filters) is in `amp.rs`. FX parameter blocks are `fx::STRIDE` wide with one segment per type: add new types at the end of `FxKind`, since patches store the type as its index. Rebuilding an FX unit when its type changes happens in `App::rebuild_fx`.

**UI and control surfaces.**
- `app.rs`: `App` state, keyboard and popup handling, patches and sessions.
- `ui.rs`: rendering, including the ordered column layout (`partition`) and the sampler's braille waveform.
- `midi.rs`: ports, the virtual "Arkeology Synth" input, CC learn mappings (applied on the UI thread in `App::on_midi`).
- `recorder.rs`: WAV recording. The engine copies output blocks into a pre-allocated pool and a writer thread saves them.
- `mcp.rs` + `app/tools.rs`: an embedded MCP server (Streamable HTTP, `127.0.0.1:7878/mcp`, Origin-checked). It forwards tool calls to the UI thread, which executes them through the same `App` methods as key presses.

**Persistence.** Data lives in `dirs::data_dir()/arkeology-synth` (`--data-dir` to override): `patches/`, `sessions/`, `samples/`, `recordings/`, `autosave.json`. The autosave is written on clean quit and restored at start (`--fresh` skips it). Factory patches are built in (`patch::factory_patches`), never written to disk. The exception is the built-in sample kits: `kitgen.rs` renders them through the engines into `samples/kits/` on first launch (re-rendered when `KIT_VERSION` changes; bump it when changing recipes), and factory Kit patches reference them by paths relative to the samples folder. `vcsl.rs` does the same for recorded CC0 kits downloaded on demand (`--fetch-kits`, curl, pinned VCSL commit). WAV loading falls back from hound to a lenient chunk parser for files hound rejects. Samples are referenced by path. A synth type's sample slots (`SynthKind::sample_slots`: 1 for granular/sampler, 16 kit pads) live in `UiSlot::samples`, and patches store them as `sample` or `samples` (keyed `padN`).

## Conventions and gotchas

- Gain staging is calibrated by measurement. When adding sounds or engines, compare peak and RMS against existing ones (single FM note ≈ 0.2 peak) and check that the `--render-demo` mix doesn't push the master soft clipper (peaks ≲ 0.9).
- Physical models must stay in tune. Tests use the YIN detector (`Sample::detect_pitch`) to assert pitch within a few cents. Loop filters' phase delay is compensated at the fundamental, and HF damping is capped in the treble (`WaveString::tune`).
- The bowed-string model is regime-sensitive (Helmholtz vs multi-slip/sticking). `bow_slope` and `bowed_loss_pole` were tuned against a grid of pressure × position × velocity × note, and `bowed_strings_find_helmholtz_motion` guards the defaults. Re-run a similar grid before changing them.
- Key repeat: terminals with the kitty keyboard protocol report `KeyEventKind::Repeat` separately. Only keys allowed by `repeatable()` in `app.rs` act on repeats.
- CI (`.github/workflows/build.yml`) runs `cargo fmt --check`, `clippy -D warnings` and the tests on Linux, macOS and Windows; keep code portable (Unix-only bits such as virtual MIDI ports sit behind `#[cfg(unix)]`). `v*` tags publish a release.
- Commit each finished, tested change on `main`; the remote is `origin` (github.com/pwwolf/arkeology-synth). Push only when asked.
