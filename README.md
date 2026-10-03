# Arkeology Synth

A multitimbral synth engine for the terminal. Run up to 16 polyphonic synths at once, each
listening on its own MIDI channel, and play a whole arrangement live from a keyboard, a
controller or a DAW/sequencer.

- **FM**: 4 operators, 8 algorithms, op-4 feedback, per-operator ADSR, ratio/detune and
  velocity sensitivity, vibrato (mod wheel adds depth).
- **Analog**: a polyphonic subtractive synth for pads, brass, stabs and leads.
  - Two PolyBLEP oscillators (saw, pulse or triangle), plus a sub oscillator, noise and
    per-note analog drift.
  - Unison stacks up to 7 detuned copies per note, spread across the stereo field.
  - The resonant low-pass switches between 12 dB (state-variable) and 24 dB (ladder),
    with its own envelope, key tracking and velocity.
  - Separate amp and filter envelopes, and a global LFO (sine, triangle, square or
    sample-and-hold) routable to pitch, cutoff and pulse width.
  - A Juno-style stereo chorus (I, II, I+II).
- **Physical**: physically modelled instruments with four models. Only the selected
  model's controls are shown.
  - **String** is an extended Karplus-Strong plucked string. A fractional all-pass keeps it
    in tune, and it has damping for brightness and dispersion for stiffness. Pick
    position shapes the excitation.
  - **Mallet** is modal synthesis: a strike rings up to 8 resonators tuned to a material's
    partials (wood/marimba, metal/vibraphone, glass, free bar, bell, membrane, tine).
  - **Piano** uses 1–3 detuned stiff strings per note, as on a real piano, so notes beat
    and decay in two stages. A felt hammer's contact time shortens as you play harder,
    making loud notes brighter. Stiffness and decay vary across the keyboard, dampers
    stop notes on key-up (the top octave and a half has none, as on a real piano), the
    sustain pedal holds, and each strike has a little hammer thump.
  - **Bowed** is a bowed-string waveguide with a nonlinear stick-slip friction junction.
    Velocity sets bow speed, Bow Pressure and the mod wheel set bow force, and you choose
    bow position, bow attack and delayed vibrato. Instrument picks a violin, viola, cello
    or bass body. Notes sustain while held, and force follows speed and position as on a
    real bow, so notes from E2 to E6 settle into a proper bowed tone at any velocity.
    The very top of the violin range (above ~E6) is less reliable.
  - Every element's phase delay is compensated, so strings, mallets and pianos measure
    within a few cents of pitch across the keyboard.
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
- **Kit**: a sample drum kit with 16 pads, each playing its own WAV/FLAC one-shot.
  - Pads default to the General MIDI drum notes (kick 36, snare 38, closed hat 42, open hat
    46, clap, rim, toms, crash, ride, pedal hat, tambourine, cowbell, shaker…). Each pad's
    note can be changed.
  - Every pad has tune, level, pan, decay, filter cutoff and a choke group. The hi-hats
    share one, so a closed hat cuts off a ringing open hat.
  - Velocity sets level (Vel Sens) and optionally brightness (Vel>Cutoff).
  - Press `f` to load a file into the pad you're editing. In the file browser, press `K` to
    load the whole folder as a kit: files are assigned to pads from their names ("kick",
    "bd", "snare", "sd", "hh", "open hat", "tom low", "crash", "ride"…), and unrecognised
    ones fill the remaining pads.
  - Kits save as patches, which record each pad's file.
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
- **Effects**: every synth has 3 insert effects (synth → FX 1 → FX 2 → FX 3 → volume/pan),
  and the master bus has 3 more after the reverb return.

  | Effect | Controls |
  |--------|----------|
  | Delay | time up to 2 s, feedback, ping-pong, feedback tone |
  | Reverb | size, damping, width, pre-delay |
  | Chorus | rate, depth |
  | Flanger | rate, depth, feedback (±) |
  | Phaser | 6-stage; rate, depth, feedback |
  | Drive | soft, hard, foldback or tube; drive, tone, output |
  | Filter | low/band/high-pass with LFO (auto-wah) |
  | EQ | 3-band: low shelf, sweepable mid, high shelf |
  | Compressor | threshold, ratio, attack, release, makeup |
  | Crusher | bit depth and sample-rate reduction |
  | Tremolo | rate, depth, sine/square, auto-pan |

  Pick an effect with the unit's **Type**. Only that effect's controls are shown, and
  **Mix** sets dry/wet (it resets to a sensible amount when you change type). Synth FX are
  saved in the patch, and master FX in the session. Effects are built off the audio
  thread, so changing type mid-performance doesn't glitch the engine.
- Every slot has volume, pan, reverb send, transpose and bend range. The polyphonic synths
  also have voice count and glide.
  The master bus has a stereo reverb and a soft-clipping drive stage.
- Built-in patches for every engine. FM: E.Piano, Bright EP, DX Bass, FM Strings, Tubular
  Bells, Glass Bell, Steel Pan, Harpsichord, FM Clav, Saw Lead, FM Flute, Ice Pad, Log Drum,
  Marimba, Organ, Brass. Granular: Frozen Choir, Vowel Morph, Glass Shimmer, Bowed Glass, Saw
  Cloud, Grain Bass, Stutter Pluck, Reverse Swell, Rain, Lo-Fi Grains, Choir Cloud, Shimmer Pad.
  Plus Warm Pad, Poly Brass, Juno Strings, Supersaw Lead,
  Poly Stab, Soft Bass, Grand/Upright/Felt/Honky-Tonk Piano, Solo Violin, Viola, Cello, Double Bass, String Section,
  Nylon/Steel Guitar, Harp, Clav, Koto, Modelled Marimba, Vibraphone, Xylophone, Church Bell,
  Kalimba, five acid basses, 808/909/Lo-Fi kits,
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

### Recording

Press `R` to record what you hear: the master output after FX, volume and drive. The
header shows a red **● REC** timer while recording. Files are 32-bit float stereo WAVs at
the device's sample rate, saved to `recordings/` and named by date and time. Earlier takes
are never overwritten, and quitting while recording finalizes the file. The audio thread
hands blocks to a writer thread through a lock-free queue, so recording can't cause
dropouts. If the disk ever falls more than ~5 s behind, the missing time is reported.

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
| `get_rack`, `get_params` | inspect slots, channels and mute/solo; every parameter with its range, default, units, scale, current value and mapped CC; and how the instrument responds to MIDI (mod wheel, velocity, sustain, bend, note layout) |
| `describe_synth` | any synth type's full control reference without adding it, noting which controls apply only to certain physical models or FX types |
| `map_cc`, `list_midi_mappings`, `clear_midi_mapping` | manage which controller knobs (channel + CC) drive which parameters, like MIDI learn |
| `set_params` | set parameters on a slot or `"master"`; values can be numbers or strings like `"250ms"`, `"2.5k"`, `"40%"`, `"c4"`, `"LowPass"` |
| `add_synth`, `remove_synth`, `set_slot` | build the rack: synth type, MIDI channel, mute/solo, name |
| `list_patches`, `load_patch`, `save_patch` | browse factory and user patches; save the current sound |
| `list_sessions`, `load_session`, `save_session`, `new_rack` | whole-rack snapshots; start an empty or starter rack |
| `load_sample`, `load_kit_folder` | load a WAV/FLAC into a granular synth, sampler (with root-note detection) or kit pad; load a folder as a kit |
| `play_notes`, `panic` | audition a sound through your speakers; stop everything |
| `start_recording`, `stop_recording` | record the output to a WAV |

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
| `N` | new rack: empty or starter (by default the current rack is saved as a session first) |
| `f` | load a WAV or FLAC into a granular synth, sampler or kit pad (`K` in the browser loads a whole folder as a kit) |
| `r` `m` `s` | rename, mute, solo |
| `c` / `C` | MIDI-learn a CC for the selected parameter / clear it |
| `p` | MIDI input ports |
| `k` | keyboard play mode (`a w s e d f t g y h u j k …` notes, `z/x` octave, `c/v` velocity, `esc` exits) |
| `space` | panic (all notes off) |
| `R` | start / stop recording the output to a WAV |
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
recordings/ WAVs recorded with `R` (or MCP start_recording)
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
| `src/synth/fm.rs`, `analog.rs`, `physical.rs`, `granular.rs`, `acid.rs`, `drums.rs`, `kit.rs`, `sampler.rs` | the eight engines |
| `src/sample.rs` | WAV/FLAC loading, built-in sources, waveform overview and onset detection |
| `src/dsp.rs`, `src/reverb.rs` | oscillator table, ADSR, SVF filter, Freeverb |
| `src/app.rs`, `src/ui.rs` | TUI state, input and rendering |
| `src/patch.rs` | patches, sessions, factory presets |
| `src/fx.rs` | insert effects (delay, reverb, modulation, drive, filter, EQ, dynamics, crusher, tremolo) |
| `src/mcp.rs`, `src/app/tools.rs` | embedded MCP server (HTTP + JSON-RPC) and its tools |

To add a synth type, write a parameter table and an engine, then add a variant to
`SynthKind` and `Instrument`. A polyphonic engine implements `Voice` and reuses `Poly` for
allocation. A mono or one-shot engine (like `acid.rs` or `drums.rs`) handles notes itself and implements
`PolyControl`.

`cargo test` covers the DSP, MIDI parsing, patch round-trips, offline rendering of both
engines, and headless TUI rendering and navigation.
