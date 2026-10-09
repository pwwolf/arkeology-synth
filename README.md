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
  - **External input**, like a Korg NTS-1's audio in: **Ext Source** picks another rack
    slot, and that slot's sound (after its effects, before its volume) becomes a sound
    source. Mute the source slot to hear only the result.
    - **Ext In** (an oscillator wave) plays it through this synth's filter and
      envelopes, gated by your notes. Every held note adds its own copy, so Voices 1
      works best for this.
    - **Ext FM** bends both oscillators' pitch with it at audio rate (fixed tones
      turn metallic and clangy).
    - **Ext Ring** ring-modulates oscillator 1 with it.
    - **Ext Gain** sets how hard the input drives all three, and it's soft-limited.
      The default puts Ext In near a saw's level.

    The input runs one 64-sample block (~1.3 ms) behind the source, so any slot can
    feed any other, even itself. Ext Source names a slot number, not a sound: a patch
    that uses it picks up whatever is in that slot of the rack it's loaded into.
    The factory patch **Audio In** is set up for this: Osc 1 = Ext In, no Osc 2, one
    voice, a resonant filter, and Ext Source = Slot 1. Ext Ring and Ext FM do nothing
    while Ext Source is Off. For example, put a drum kit in slot 1 and mute it. Then in slot 2, an Analog with
    Ext Source = Slot 1, Osc 1 = Ext In and a resonant filter plays the drums through
    the filter, keyed by your notes. Or set Ext Ring at 100% on a saw lead for clangy,
    rhythmic tones.
- **Physical**: physically modelled instruments with four models. Only the selected
  model's controls are shown.
  - **String** is an extended Karplus-Strong plucked string. A fractional all-pass keeps it
    in tune, and it has damping for brightness and dispersion for stiffness. Pick
    position shapes the excitation. For guitars:
    - **Decay Track** makes high notes ring shorter than low ones, as on a real guitar
      (at 100%, each octave up halves the decay).
    - **Two-Stage Decay** models the string's two vibration planes. One drives the
      bridge hard and dies quickly, the other rings on, slightly detuned. The result is
      a prompt attack, then a long, gently beating tail.
    - **Body Type** picks the resonating body: Box (a generic three-resonance box),
      or a modal Dreadnought, Classical or Parlor body. Each modal body has 14
      resonances: the sound-hole air mode, the top plate's breathing modes and
      higher plate modes. **Body** sets how much of it you hear.
    - **Sympathetic** adds six open strings in standard tuning (E2 to E4). The notes
      you play drive them through the bridge, and they ring on at matching pitches: a
      quiet halo that keeps a guitar resonating after a note is damped.
    - For long sustain, turn **Decay** up: it's the time to fall 60 dB, so a real
      steel string's low notes need 12–20 s. Lower **Release Damp** lets short key
      presses ring on instead of muting them.
    - New controls default to the earlier sound, so existing patches are unchanged. The
      Steel, Nylon and Parlor Guitar patches use them all; Harp, Koto and the electric
      guitars use the decay features.
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
- **Tonewheel organ**: a drawbar organ with a rotary speaker.
  - Nine drawbars (16' to 1', 0–8, 3 dB per step) mix free-running sine "tonewheels".
    Pitches above the top wheel fold back an octave, as on the real instrument.
  - Percussion (second or third harmonic, soft/normal, fast/slow decay) is single-trigger:
    it sounds only when no other key is held, so legato playing stays smooth.
  - Key click, scanner vibrato and chorus (V1–V3, C1–C3) and tube-style overdrive.
  - The rotary speaker splits at 800 Hz into a horn and a drum rotor. Each rotor has its
    own Doppler shift, amplitude modulation and inertia, and is heard from two
    microphones for stereo. The light horn changes speed in under a second, the heavy
    drum takes a few. **The mod wheel switches it to fast**, or set Speed to Fast.
  - Velocity is ignored, as on a real organ. For a swell pedal, MIDI-learn a CC to Volume.
  - Factory patches: Jazz, Gospel, Rock, Ballad, Full and Church Organ.
- **Vocal**: a formant voice for choirs, solo voices and talkbox leads.
  - Each note is one to six **singers**. Each has a glottal pulse (or a saw or buzz
    for talkbox and robot sounds), breath noise, vibrato that fades in after a delay,
    and slow random drift in pitch and level. Ensemble singers are detuned, spread
    across the stereo field and come in at slightly different moments.
  - Five formant resonators in series shape the vowel, from the classic soprano,
    alto, tenor and bass vowel tables. **Vowel** morphs smoothly through a → e → i →
    o → u, **Hum** closes the mouth to "mmm", and **Formant Shift** makes the voice
    sound bigger or smaller without changing pitch.
  - **The mod wheel moves the vowel** (Wheel>Vowel sets how far), so a controller can
    make it talk. Velocity sets level and brightness.
  - It sings vowels, not words.
  - Factory patches: Choir Aah, Choir Ooh (the wheel opens it to "aah"), Soprano Solo,
    Bass Monks, Talkbox Lead, Robot Voice and Hum Pad.
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
  - **Built-in kits** are rendered from the synth's own engines into `samples/kits/` on first
    launch, so they're free of licensing concerns and take no space in the repo:
    - **808**, **909** and **Lo-Fi**, from the drum machine.
    - **Hybrid**: machine drums layered with physically modelled membranes and bars, then
      compressed, driven and given a short room.
    - **Hand Percussion**: congas, bongos, djembe, woodblock, clave, agogô, triangle,
      tambourine, shaker, cowbell and log drum, on the General MIDI percussion notes.

    Load them as the factory patches **808 Sampled**, **909 Sampled**, **Lo-Fi Sampled**,
    **Hybrid Kit** and **Hand Percussion**. Use `--render-kits` to re-render them.
  - **Recorded acoustic kits** come from the
    [Versilian Community Sample Library](https://github.com/sgossner/VCSL) (CC0, public
    domain). Run `mise run fetch-kits` (or `--fetch-kits`) once to download about 12 MB of
    curated one-shots from a pinned VCSL commit. You then get **VCSL Acoustic Kit**
    (snare, hats, stick toms, cymbals, cross-stick, rimshot…) and **VCSL Percussion**
    (congas, bongos, agogôs, claves, woodblocks, cabasa, shaker, triangle…), both on
    General MIDI notes. VCSL is an orchestral library, so the kick is a concert bass drum,
    shortened to sit in a kit. The patch browser tags these kits "download" until fetched.
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
  | Reverb | room type (see below), size, damping, width, pre-delay |
  | Chorus | rate, depth |
  | Flanger | rate, depth, feedback (±) |
  | Phaser | 6-stage; rate, depth, feedback |
  | Drive | soft, hard, foldback or tube; drive, tone, output |
  | Filter | low/band/high-pass (12 or 24 dB/oct) with LFO (auto-wah) |
  | EQ | 3-band: low shelf, sweepable mid, high shelf |
  | Compressor | threshold, ratio, attack, release, makeup |
  | Crusher | bit depth and sample-rate reduction |
  | Tremolo | rate, depth, sine/square, auto-pan |
  | Pedal | guitar stompbox: Overdrive (Tube Screamer-style mid-boosted soft clipping), Distortion (RAT-style hard clipping), Fuzz (asymmetric, Fuzz Face-style); drive, tone, level |
  | Amp | guitar amp: Clean, Crunch, Lead or High Gain preamp (2–4 tube stages), Bass/Mid/Treble tone stack, power amp with sag, Presence, and a speaker cabinet (1×12 open, 2×12, 4×12 closed, or off); gain, level |

  Put a Pedal before an Amp for a full rig, as on a pedalboard. The Pedal and Amp
  clipping stages run 4× oversampled, so even bright synth sounds distort without
  aliasing. The Gain knob mostly changes the amount of distortion, not the volume. The
  factory patches **Electric Clean**, **Crunch Guitar**, **Distortion Guitar** and
  **High Gain Rhythm** play the physical String model through the rig, and **Crunch
  Organ** and **Fuzz Lead** do the same for the organ and an analog lead.

  Pick an effect with the unit's **Type**. Only that effect's controls are shown, and
  **Mix** sets dry/wet (it resets to a sensible amount when you change type). Synth FX are
  saved in the patch, and master FX in the session. Effects are built off the audio
  thread, so changing type mid-performance doesn't glitch the engine.
- Every slot has volume, pan, reverb send, transpose and bend range. The polyphonic synths
  also have voice count and glide.
  The master bus has a stereo reverb and a soft-clipping drive stage.
- Reverb types, on the master reverb (**Type**) and the Reverb insert (**Room**):

  | Type | Character | Decay at default Size |
  |---|---|---|
  | Classic | the original Freeverb-style reverb (older sessions keep it) | ~2 s |
  | Room | fast, dense early reflections, short and darker | ~0.7 s |
  | Chamber | thick, smooth build-up | ~1.5 s |
  | Hall | sparse early reflections after a pre-delay, long smooth tail | ~3 s |
  | Cathedral | very long, dark, slow to build | ~7 s |
  | Plate | no early reflections, an instant bright wash | ~2.2 s |
  | Spring | a guitar-amp spring tank: thin, with the chirpy "boing" | ~2.5 s |

  Size scales the space and its decay within each type's range, and Damping sets how
  much faster the treble dies (specified at 4 kHz). All types except Classic and
  Spring use one design: an eight-line modulated feedback delay network with
  per-type early reflections, pre-delay and tone. Spring is a separate dispersive
  all-pass model. The types are level-matched, so switching type doesn't change the
  send level. New starter racks use Hall.
- Built-in patches for every engine. FM: E.Piano, Bright EP, DX Bass, FM Strings, Tubular
  Bells, Glass Bell, Steel Pan, Harpsichord, FM Clav, Saw Lead, FM Flute, Ice Pad, Log Drum,
  Marimba, Organ, Brass. Granular: Frozen Choir, Vowel Morph, Glass Shimmer, Bowed Glass, Saw
  Cloud, Grain Bass, Stutter Pluck, Reverse Swell, Rain, Lo-Fi Grains, Choir Cloud, Shimmer Pad.
  Bass: DnB Sub and Reese Mid (layer them on one channel), Reese, Neuro Growl, Wobble Bass.
  Plus Warm Pad, Poly Brass, Juno Strings, Supersaw Lead,
  Poly Stab, Soft Bass, Grand/Upright/Felt/Honky-Tonk Piano, Solo Violin, Viola, Cello, Double Bass, String Section,
  Nylon/Steel Guitar, Harp, Clav, Koto, Modelled Marimba, Vibraphone, Xylophone, Church Bell,
  Kalimba, five acid basses, 808/909/Lo-Fi kits,
  sampler examples (Choir Loop, Pluck Slices, Saw Stab, Reverse Glass) and more. Press `l`
  to browse them (`tab` switches synth type, `space` auditions). Edit one and press `w` to
  save your own version.
- You can save and load patches (one synth) and sessions (the whole rack, plus MIDI
  mappings and ports). The rack is autosaved on quit and restored on the next start.
- Program change: each synth listening on that channel loads the patch with that number
  *for its own synth type*, so an organ slot steps through organ patches and a kit slot
  through kits. The patch browser shows the numbers (1–128). Factory patches are
  numbered first, so their numbers never change; your saved patches follow,
  alphabetically, so saving a new one can renumber your others.
  Press `P` on a slot to make it ignore program changes ("Rx Program Change" off), so a
  sequencer can't replace a sound you set up by hand. The setting is saved with the
  session. If a program change does replace a sound you'd edited but not saved, the
  status line says so.
- MIDI learn: map any CC to any parameter. Continuous parameters glide to each new
  value over about 20 ms (log-scaled ones like cutoff glide evenly in pitch), so knob
  sweeps from 7-bit CCs, key presses and MCP don't zipper. Switches, choices and
  whole-number settings change instantly.
- A spectrum analyzer of the master output appears under the rack while sound plays
  and hides a couple of seconds after it stops. It shows 30 Hz–16 kHz in log-spaced
  bands with peak hold, tilted +3 dB/octave so a typical mix reads roughly flat. It
  only appears if the rack still fits on screen. `v` turns it off and on.

## Running

```sh
cargo run --release
cargo run --release -- --help          # all options
cargo run --release -- --list-devices  # audio outputs and MIDI inputs
```

To audition every factory patch without the TUI, run `mise run render-patches` (or
`--render-patches DIR`, optionally with `--only TEXT` to filter by name or synth type).
Each patch plays a phrase that suits it into its own WAV: chords and a melody, a bass
riff, a 303 line, a drum groove, or every kit pad in turn. Then `levels.tsv` lists
every patch's peak and RMS level and flags any that are silent, very quiet or close
to clipping. All ~115 patches render in a few seconds.

On startup the synth connects to every MIDI input it finds. On macOS and Linux it also opens
a virtual input called **Arkeology Synth**: point DAW tracks at it, one MIDI channel per
synth, to sequence a song.

With no MIDI hardware attached, press `k` to play the selected synth from the computer
keyboard. To send a test phrase to the virtual port, run
`cargo run --release --example midi_send`.

### Prebuilt binaries

GitHub Actions (`.github/workflows/build.yml`) tests and builds every push to `main` and
every pull request on Linux (x86_64), macOS (one universal binary for Apple Silicon and
Intel) and Windows (x86_64). Download the builds from a workflow run's **Artifacts**.
Pushing a version tag publishes them as a GitHub Release:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The binaries aren't code-signed:

- **macOS** quarantines downloaded binaries. After unpacking, run
  `xattr -d com.apple.quarantine arkeology-synth`.
- **Windows** SmartScreen may warn on first launch: choose **More info → Run anyway**.
  Virtual MIDI ports don't exist on Windows, so connect a hardware port, or a loopback
  driver such as loopMIDI, for DAW input.
- **Linux** needs ALSA at runtime (`libasound2`, installed on almost every desktop distro).

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
| `add_synth`, `remove_synth`, `set_slot` | build the rack: synth type, MIDI channel, mute/solo, program-change lock, name |
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
| `L` / `W` | load / write a session (`W` offers the session you last loaded or saved, so `W` Enter saves over it) |
| `N` | new rack: empty or starter (by default the current rack is saved as a session first) |
| `f` | load a WAV or FLAC into a granular synth, sampler or kit pad (`K` in the browser loads a whole folder as a kit) |
| `r` `m` `s` | rename, mute, solo |
| `v` | spectrum analyzer on / off |
| `P` | receive or ignore MIDI program change on this slot (a struck-out P means locked) |
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

Velocity follows a curve shared by every engine: at 100% sensitivity (Vel Sens or
Vel>Amp) it's the General MIDI curve, so a note played at velocity 64 is about 12 dB
quieter than a full-force one, and at 32 about 24 dB quieter. Hard playing (90–127)
stays near full level. Lower sensitivity narrows the range. The organ ignores
velocity, as a real one does, and the 303 uses it only for accents.

MIDI timing is sample-accurate by default. Each message is stamped when it arrives and
placed at the matching sample of the next audio buffer, so notes from a DAW or
sequencer keep their exact spacing, at a constant delay of one buffer (about 5 ms at 256
frames). `--midi-timing immediate` applies notes at the start of the next buffer
instead. That's the lowest latency for live playing, but timing can drift by up to a
buffer.

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
| `src/synth/fm.rs`, `analog.rs`, `physical.rs`, `tonewheel.rs`, `vocal.rs`, `granular.rs`, `acid.rs`, `drums.rs`, `kit.rs`, `sampler.rs` | the ten engines |
| `src/sample.rs` | WAV/FLAC loading, built-in sources, waveform overview and onset detection |
| `src/dsp.rs`, `src/reverb.rs` | oscillator table, ADSR, SVF filter, biquads; reverb types (Freeverb, FDN, spring) |
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
