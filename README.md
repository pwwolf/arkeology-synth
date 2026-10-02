# Arkeology Synth

A multitimbral synth engine for the terminal. Run up to 16 polyphonic synths at once, each
listening on its own MIDI channel, and play a whole arrangement live from a keyboard, a
controller or a DAW/sequencer.

- **FM**: 4 operators, 8 algorithms, op-4 feedback, per-operator ADSR, ratio/detune and
  velocity sensitivity, vibrato (mod wheel adds depth).
- **Granular**: a grain cloud per voice over a WAV/FLAC file or one of five built-in sources
  (Choir, Glass, Saw, Pluck, Noise). Controls for position, spray, scan speed, size,
  density, jitter, pitch spray, stereo spread, reverse probability and window shape, plus
  an amp envelope and a multimode filter. The mod wheel adds spray.
- **Acid (303-style)**: a monophonic bass voice. A saw or square wave runs into a resonant
  4-pole ladder filter with an envelope-modulated cutoff, plus decay, drive and tuning.
  Accent and slide come from how you play, as on the original. Notes at or above the
  "Accent Vel" threshold are accented: louder, with a snappier envelope and an extra filter
  sweep that builds up over consecutive accents. A note that starts while the previous one
  is still held slides to the new pitch without retriggering. In a DAW, overlap notes to
  get slides.
- **Drums (808/909-style)**: a fully synthesized kit that plays the General MIDI drum map,
  so any DAW drum track or pad controller works without remapping. New drum synths default
  to channel 10.

  | Drum | Notes | Sound |
  |------|-------|-------|
  | Kick | 35, 36 | sine with a pitch sweep (Punch) |
  | Rim | 37 | two short tuned partials |
  | Snare | 38, 40 | two tones plus filtered noise (Snappy) |
  | Clap | 39 | noise bursts plus a tail (Spread) |
  | Closed / pedal hat | 42, 44 | six detuned squares, band-passed; chokes the open hat |
  | Open hat | 46 | the same, with a longer decay |
  | Toms (low / mid / high) | 41 43 / 45 47 / 48 50 | sines with a pitch drop |
  | Cowbell | 56 | two squares through a band-pass |
  | Cymbal | 49, 51, 52, 55, 57, 59 | metallic squares plus noise |

  Each drum has Tune, Decay, a tone control and Level. The kit also has velocity
  sensitivity and drive, and Transpose/pitch bend retune the whole kit. In keyboard play
  mode a drum slot starts at the kick, so the home row plays the kit (`a` kick, `s` snare,
  `e` clap, `t` closed hat, `u` open hat).
- **Sampler**: plays a WAV or FLAC file (mono or stereo) or a built-in source, with a waveform view
  above its parameters. Three modes:
  - **Classic**: pitched across the keyboard from a Root Note, with an optional
    crossfaded sustain loop. Loop points can be moved while notes play. When you load a
    file, its pitch is detected (YIN) and Root Note and Tune are set so it plays in tune.
    Unpitched material such as drums or full mixes leaves them unchanged.
  - **One-shot**: each note plays the whole start–end region and ignores note-off.
  - **Slice**: cuts the region into equal slices, or at detected transients
    (Sensitivity sets how many), mapped to consecutive notes from First Slice. Load a drum
    break and each pad plays one hit.

  It also has start/end, reverse, tune, an amp envelope, a multimode filter, and
  velocity control of level and cutoff. Playback uses 4-point Hermite interpolation. Press
  `f` to load a WAV or FLAC. Root Note and First Slice accept names like `c4` or `f#2`. In keyboard play mode a
  sliced sampler starts at its first slice.
- Every slot has volume, pan, reverb send, transpose and bend range. The polyphonic synths
  also have voice count and glide.
  The master bus has a stereo reverb and a soft-clipping drive stage.
- Built-in patches for every engine: FM Strings, E.Piano, Glass Bell, Marimba, Soft Pad,
  Brass, Organ, Mono Lead, Choir Cloud, Shimmer Pad, five acid basses, 808/909/Lo-Fi kits,
  sampler examples (Choir Loop, Pluck Slices, Saw Stab, Reverse Glass) and more. Press `l`
  to browse them (`tab` switches synth type, `space` auditions). Edit one and press `w` to
  save your own version.
- You can save and load patches (one synth) and sessions (the whole rack, plus MIDI
  mappings and ports). The rack is autosaved on quit and restored on the next start.
- MIDI learn: map any CC to any parameter.

## Running

```sh
cargo run --release
cargo run --release -- --help          # all options
cargo run --release -- --list-devices  # audio outputs and MIDI inputs
```

On startup the synth connects to every MIDI input it finds. On macOS and Linux it also opens
a virtual input called **Arkeology Synth**: point DAW tracks at it, one MIDI channel per
synth, to sequence a song.

With no MIDI hardware attached, press `k` to play the selected synth from the computer
keyboard. To send a test phrase to the virtual port, run
`cargo run --release --example midi_send`.

`--render-demo out.wav` renders a short multitimbral demo offline (no audio device needed).

## MCP server

The synth runs an MCP server on `http://127.0.0.1:7878/mcp` (Streamable HTTP transport), so
an assistant like Claude Code can inspect and configure it while you play. To connect
Claude Code:

```sh
claude mcp add --transport http arkeology http://127.0.0.1:7878/mcp
```

| Tool | What it does |
|------|--------------|
| `get_rack`, `get_params` | inspect slots, channels, mute/solo, and every parameter with its range and current value |
| `set_params` | set parameters on a slot or `"master"`; values can be numbers or strings like `"250ms"`, `"2.5k"`, `"40%"`, `"c4"`, `"LowPass"` |
| `add_synth`, `remove_synth`, `set_slot` | build the rack: synth type, MIDI channel, mute/solo, name |
| `list_patches`, `load_patch`, `save_patch` | browse factory and user patches; save the current sound |
| `list_sessions`, `load_session`, `save_session` | whole-rack snapshots |
| `load_sample` | load a WAV/FLAC into a granular synth or sampler (with root-note detection) |
| `play_notes`, `panic` | audition a sound through your speakers; stop everything |

Changes apply live, through the same code as the keyboard, and the status line shows what
the client did. The server only listens on localhost and rejects browser requests from
other origins. Use `--mcp-port` to change the port (for example, when running two
instances) or `--no-mcp` to turn it off.

## Keys

| Key | Action |
|-----|--------|
| `↑ ↓` | select the master section or a synth slot / select a parameter |
| `tab` | switch between the rack and the parameter editor |
| `← →` | rack: change MIDI channel (omni, 1–16) · params: adjust (`shift` coarse, `,` `.` fine) |
| `enter` | type an exact value (`250ms`, `2.5k`, `40%`, `bandpass` …) |
| `⌫` | params: reset to default · rack: remove synth |
| `a` | add a synth |
| `l` / `w` | browse/load patches (factory and yours; `tab` filters by synth, `space` auditions) / write the selected synth's patch |
| `L` / `W` | load / write a session |
| `f` | load a WAV or FLAC into a granular synth or sampler |
| `r` `m` `s` | rename, mute, solo |
| `c` / `C` | MIDI-learn a CC for the selected parameter / clear it |
| `p` | MIDI input ports |
| `k` | keyboard play mode (`a w s e d f t g y h u j k …` notes, `z/x` octave, `c/v` velocity, `esc` exits) |
| `space` | panic (all notes off) |
| `?` | help · `q` quit |

Most terminals don't report key releases, so in keyboard mode notes auto-release about
0.6 s after the last key repeat. Terminals that support the kitty keyboard protocol
(kitty, WezTerm, Ghostty, foot, recent iTerm2) get real note-offs.

## MIDI

Each slot responds on its channel, or on every channel when set to omni. Several slots on
the same channel layer together. Per slot, the synth handles note on/off with velocity,
pitch bend, CC1 (mod wheel), CC64 (sustain) and CC120/123 (all notes off). Any other CC
can be mapped with MIDI learn.

## Files

Data lives in `~/Library/Application Support/arkeology-synth` (macOS) or
`~/.local/share/arkeology-synth` (Linux). Use `--data-dir` to change it.

```
patches/    your saved patches, one JSON file each (factory patches are built in)
sessions/   whole-rack sessions
samples/    the sample browser starts here; drop WAV/FLAC files in
autosave.json
```

Parameters are stored by name, so patch files are easy to edit by hand and keep working
when parameters are added later.

## Architecture

```
MIDI thread (midir) ─┬─► command queue ─► audio thread: Engine
                     └─► UI queue (monitor, learn, CC maps)      │ 16 slots × Poly<Voice>
UI thread (ratatui) ───► command queue ──────────────────────────┘ reverb send, master
                    ◄─── garbage queue (things to free) ◄────────
                    ◄─── atomics (meters, voice counts, CPU)
```

The audio callback never locks and never frees memory. Synths are built on the UI thread
and moved into the engine; anything the engine replaces is sent back for the UI thread to
drop. Each slot holds a flat `Vec<f32>` of parameters described by static `ParamDesc`
tables, and those tables also drive the generic TUI editor and patch serialisation.

| File | Contents |
|------|----------|
| `src/engine.rs` | slots, MIDI routing, mixer, telemetry |
| `src/synth/mod.rs` | `SynthKind`, common params, the `Poly`/`Voice` voice allocator (stealing, sustain, glide) |
| `src/synth/fm.rs`, `granular.rs`, `acid.rs`, `drums.rs`, `sampler.rs` | the five engines |
| `src/sample.rs` | WAV/FLAC loading, built-in sources, waveform overview and onset detection |
| `src/dsp.rs`, `src/reverb.rs` | oscillator table, ADSR, SVF filter, Freeverb |
| `src/app.rs`, `src/ui.rs` | TUI state, input and rendering |
| `src/patch.rs` | patches, sessions, factory presets |
| `src/mcp.rs`, `src/app/tools.rs` | embedded MCP server (HTTP + JSON-RPC) and its tools |

To add a synth type, write a parameter table and an engine, then add a variant to
`SynthKind` and `Instrument`. A polyphonic engine implements `Voice` and reuses `Poly` for
allocation. A mono or one-shot engine (like `acid.rs` or `drums.rs`) handles notes itself and implements
`PolyControl`.

`cargo test` covers the DSP, MIDI parsing, patch round-trips, offline rendering of both
engines, and headless TUI rendering and navigation.
