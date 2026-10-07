mod amp;
mod app;
mod audio;
mod audition;
mod dsp;
mod engine;
mod fx;
mod kitgen;
mod mcp;
mod midi;
mod params;
mod patch;
mod recorder;
mod reverb;
mod sample;
mod spectrum;
mod synth;
mod ui;
mod vcsl;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossbeam_queue::ArrayQueue;
use ratatui::crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};

use crate::app::{App, AppInit};
use crate::engine::{Command, CommandQueue, Engine, GarbageQueue, Telemetry};
use crate::midi::{MidiManager, MidiMsg, Sink};
use crate::patch::{Session, Storage, read_json};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum MidiTiming {
    Tight,
    Immediate,
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Arkeology: a multitimbral MIDI synth for the terminal",
    long_about = "Arkeology: a multitimbral MIDI synth for the terminal. Run up to 16 synths at \
once, one per MIDI channel: FM, analog, physical models (guitar, piano, strings, mallets), a \
tonewheel organ, a vocal choir/talkbox, granular, a 303-style bass, drum machines, sample kits and a sampler, with \
insert effects (including guitar pedals and amps), recording and an MCP server for AI \
assistants."
)]
struct Args {
    /// Audio output device (substring match; see --list-devices).
    #[arg(long)]
    device: Option<String>,
    /// Audio buffer size in frames (smaller = lower latency).
    #[arg(long)]
    buffer: Option<u32>,
    /// List audio output devices and MIDI inputs, then exit.
    #[arg(long)]
    list_devices: bool,
    /// Where patches, sessions and samples are stored.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Session file to load at startup (instead of the autosave).
    #[arg(long)]
    session: Option<PathBuf>,
    /// Start with the default rack, ignoring the autosave.
    #[arg(long)]
    fresh: bool,
    /// Don't connect to MIDI ports automatically.
    #[arg(long)]
    no_midi: bool,
    /// Port for the embedded MCP server (http://127.0.0.1:PORT/mcp).
    #[arg(long, default_value_t = mcp::DEFAULT_PORT)]
    mcp_port: u16,
    /// Don't start the embedded MCP server.
    #[arg(long)]
    no_mcp: bool,
    /// MIDI timing: `tight` places each note at its exact sample (with a
    /// constant one-buffer delay); `immediate` applies notes at the start of
    /// the next buffer (lowest latency, but jitter of up to one buffer).
    #[arg(long, value_enum, default_value_t = MidiTiming::Tight)]
    midi_timing: MidiTiming,
    /// Don't create the "Arkeology Synth" virtual MIDI input.
    #[arg(long)]
    no_virtual: bool,
    /// Download the acoustic VCSL kits (CC0, ~10 MB) into samples/kits/ and exit.
    #[arg(long)]
    fetch_kits: bool,
    /// Re-render the built-in sample kits (samples/kits/) and exit.
    #[arg(long)]
    render_kits: bool,
    /// Render a short demo to a WAV file without opening audio or the TUI.
    #[arg(long, value_name = "WAV")]
    render_demo: Option<PathBuf>,
    /// Render every factory patch through a short phrase into DIR (one WAV
    /// each, plus levels.tsv flagging silent, quiet or clipping patches).
    #[arg(long, value_name = "DIR")]
    render_patches: Option<PathBuf>,
    /// With --render-patches: only patches whose name or synth type contains this.
    #[arg(long, value_name = "TEXT", requires = "render_patches")]
    only: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    dsp::init_tables();

    if args.list_devices {
        println!("Audio outputs:");
        for d in audio::list_devices()? {
            println!("  {d}");
        }
        println!("MIDI inputs:");
        for p in MidiManager::available_ports() {
            println!("  {p}");
        }
        return Ok(());
    }

    let builtins = sample::builtins();

    if let Some(path) = &args.render_demo {
        return render_demo(path, &builtins);
    }

    let storage = Storage::new(args.data_dir.clone().unwrap_or_else(Storage::default_root))?;
    if args.fetch_kits {
        println!("Downloading acoustic kits from the Versilian Community Sample Library (CC0)…");
        let (fetched, present) = vcsl::fetch(&storage.samples_dir(), |f| println!("  {f}"))?;
        println!(
            "done: {fetched} downloaded, {present} already present, in {}",
            kitgen::kits_dir(&storage.samples_dir()).display()
        );
        println!("Load them with the \"VCSL Acoustic Kit\" and \"VCSL Percussion\" patches.");
        return Ok(());
    }
    if let Some(dir) = &args.render_patches {
        // Kit patches play the rendered kits: make sure they exist.
        kitgen::ensure(&storage.samples_dir(), &builtins, false)?;
        let started = std::time::Instant::now();
        let results =
            audition::render_patches(dir, args.only.as_deref(), &storage.samples_dir(), &builtins)?;
        let rendered = results.iter().filter(|r| r.file.is_some()).count();
        println!(
            "rendered {rendered} patches into {} in {:.1}s",
            dir.display(),
            started.elapsed().as_secs_f32()
        );
        for r in &results {
            if let Some(flag) = r.flag() {
                println!("  {flag:<20} {} {}", r.kind.label(), r.name);
            } else if r.file.is_none() {
                println!(
                    "  {:<20} {} {}: {}",
                    "SKIPPED",
                    r.kind.label(),
                    r.name,
                    r.note
                );
            }
        }
        println!(
            "levels for every patch: {}",
            dir.join("levels.tsv").display()
        );
        return Ok(());
    }
    if args.render_kits {
        let n = kitgen::ensure(&storage.samples_dir(), &builtins, true)?;
        println!(
            "rendered {n} one-shots into {}",
            kitgen::kits_dir(&storage.samples_dir()).display()
        );
        return Ok(());
    }
    // First launch (or new recipes): render the built-in sample kits.
    let kits_note = match kitgen::ensure(&storage.samples_dir(), &builtins, false) {
        Ok(0) => None,
        Ok(n) => Some(Ok(format!(
            "rendered {n} drum one-shots into {}",
            kitgen::kits_dir(&storage.samples_dir()).display()
        ))),
        Err(e) => Some(Err(format!("rendering built-in kits: {e:#}"))),
    };

    let commands: CommandQueue = Arc::new(ArrayQueue::new(4096));
    let garbage: GarbageQueue = Arc::new(ArrayQueue::new(256));
    let telemetry = Arc::new(Telemetry::default());
    let errors = Arc::new(ArrayQueue::new(64));
    let midi_ui: Arc<ArrayQueue<MidiMsg>> = Arc::new(ArrayQueue::new(1024));

    let audio = {
        let (c, g, t) = (commands.clone(), garbage.clone(), telemetry.clone());
        audio::start(
            args.device.as_deref(),
            args.buffer,
            errors.clone(),
            move |sr| Engine::new(sr, c, g, t),
        )?
    };

    let mut midi = MidiManager::new(Sink {
        engine: commands.clone(),
        ui: midi_ui.clone(),
        stamp: args.midi_timing == MidiTiming::Tight,
    });
    let mut startup_notes = Vec::new();
    if !args.no_virtual
        && let Err(e) = midi.open_virtual()
    {
        startup_notes.push(format!("{e:#}"));
    }
    if !args.no_midi {
        midi.connect_all();
    }

    // Start the TUI. Probe for key-release support before entering raw mode output.
    let mut terminal = ratatui::init();
    let has_release = terminal::supports_keyboard_enhancement().unwrap_or(false);
    if has_release {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        );
    }

    let mut app = App::new(AppInit {
        sample_rate: audio.sample_rate,
        device_name: audio.device_name.clone(),
        storage,
        builtins,
        commands,
        garbage,
        telemetry,
        midi_in: midi_ui,
        errors,
        midi,
        has_key_release: has_release,
    });

    let session_path = args.session.clone().or_else(|| {
        let p = app.storage.autosave_path();
        (!args.fresh && p.exists()).then_some(p)
    });
    match session_path {
        Some(p) => match read_json::<Session>(&p) {
            Ok(s) => {
                app.apply_session(s);
                if args.session.is_some() {
                    app.session_name = p.file_stem().map(|s| s.to_string_lossy().into_owned());
                }
                app.info(format!("restored {}", p.display()));
            }
            Err(e) => {
                app.default_rack();
                app.error(format!("{e:#}"));
            }
        },
        None => {
            app.default_rack();
            app.info("welcome! press ? for help, k to play from the keyboard");
        }
    }
    if !args.no_mcp {
        let (tx, rx) = std::sync::mpsc::channel();
        match mcp::start(args.mcp_port, tx) {
            Ok(addr) => {
                app.mcp = Some(rx);
                app.mcp_addr = Some(addr);
            }
            Err(e) => {
                startup_notes.push(format!("{e:#} (another instance running? try --mcp-port)"))
            }
        }
    }
    match kits_note {
        Some(Ok(msg)) => app.info(msg),
        Some(Err(e)) => startup_notes.push(e),
        None => {}
    }
    if let Some(n) = startup_notes.pop() {
        app.error(n);
    }

    let result = run(&mut terminal, &mut app);

    // Finalize any recording while the audio stream is still running.
    app.finish_recording(Duration::from_secs(5));
    app.autosave();
    if has_release {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    drop(audio);
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(16))? {
            // Drain everything pending so fast key repeats don't lag the UI.
            loop {
                if let Event::Key(key) = event::read()? {
                    app.on_key(key);
                }
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        app.tick();
    }
    Ok(())
}

/// Offline render: FM, granular and two bass slots (FM and 303) playing a short phrase.
fn render_demo(path: &PathBuf, builtins: &sample::Builtins) -> Result<()> {
    use crate::midi::MidiKind;

    let sr = 48_000.0;
    let commands: CommandQueue = Arc::new(ArrayQueue::new(4096));
    let garbage: GarbageQueue = Arc::new(ArrayQueue::new(256));
    let mut engine = Engine::new(
        sr,
        commands.clone(),
        garbage,
        Arc::new(Telemetry::default()),
    );
    let patches = patch::factory_patches();
    let get = |n: &str| {
        patches
            .iter()
            .find(|p| p.name == n)
            .expect("factory patch")
            .clone()
    };
    let rack = [
        ("E.Piano", 0u8),
        ("Choir Cloud", 1),
        ("Solid Bass", 2),
        ("Acid Squelch", 3),
        ("909 Kit", 9),
    ];
    for (slot, (name, channel)) in rack.iter().enumerate() {
        let p = get(name);
        let mut values = p.values();
        values[synth::VOLUME] *= 0.8;
        let data = engine::Slot::new(p.kind, values, Some(*channel), sr, builtins, &[]);
        let _ = commands.push(Command::InstallSlot { slot, data });
    }

    // (time in beats, channel, note, velocity; 0 = note off)
    let mut events: Vec<(f32, u8, u8, u8)> = Vec::new();
    let chords: [[u8; 3]; 4] = [[60, 64, 67], [57, 60, 64], [53, 57, 60], [55, 59, 62]];
    let bass = [36u8, 33, 29, 31];
    for (bar, chord) in chords.iter().enumerate() {
        let t = bar as f32 * 4.0;
        for (k, &n) in chord.iter().enumerate() {
            events.push((t + k as f32 * 0.5, 0, n + 12, 96));
            events.push((t + 3.5, 0, n + 12, 0));
            events.push((t, 1, n, 96));
            events.push((t + 3.9, 1, n, 0));
        }
        events.push((t, 2, bass[bar] + 12, 96));
        events.push((t + 1.5, 2, bass[bar] + 12, 0));
        events.push((t + 2.0, 2, bass[bar] + 12, 96));
        events.push((t + 3.0, 2, bass[bar] + 12, 0));
    }
    // A 16th-note acid line on channel 4: velocity >= 100 accents, and
    // notes held into the next step slide.
    let line: [(u8, u8, bool); 16] = [
        (36, 110, false),
        (36, 80, false),
        (48, 80, true),
        (36, 80, false),
        (39, 110, false),
        (36, 80, false),
        (46, 80, true),
        (48, 110, false),
        (36, 80, false),
        (36, 110, false),
        (43, 80, true),
        (41, 80, false),
        (39, 80, false),
        (36, 110, false),
        (48, 80, true),
        (36, 80, false),
    ];
    for rep in 0..4 {
        for (step, &(note, vel, slide)) in line.iter().enumerate() {
            let t = rep as f32 * 4.0 + step as f32 * 0.25;
            events.push((t, 3, note, vel));
            events.push((t + if slide { 0.3 } else { 0.15 }, 3, note, 0));
        }
    }
    // Drums on channel 10: kick on every beat, clap + snare on 2 and 4,
    // closed hats on 8ths, an open hat on the last off-beat, a fill at the end.
    for beat in 0..16 {
        let t = beat as f32;
        let mut hit = |dt: f32, note: u8, vel: u8| {
            events.push((t + dt, 9, note, vel));
            events.push((t + dt + 0.1, 9, note, 0));
        };
        hit(0.0, 36, 120);
        if beat % 2 == 1 {
            hit(0.0, 38, 110);
            hit(0.0, 39, 90);
        }
        hit(0.0, 42, 70);
        if beat % 4 == 3 {
            hit(0.5, 46, 85);
        } else {
            hit(0.5, 42, 55);
        }
        if beat == 15 {
            hit(0.25, 48, 100);
            hit(0.5, 45, 100);
            hit(0.75, 41, 110);
        }
    }
    events.push((16.0, 9, 49, 110));
    events.push((16.1, 9, 49, 0));
    events.sort_by(|a, b| a.0.total_cmp(&b.0));

    let bpm = 100.0;
    let total_beats = 20.0;
    let frames_per_beat = sr * 60.0 / bpm;
    let total = (total_beats * frames_per_beat) as usize;
    let mut out = vec![0.0f32; total * 2];
    let mut ev = events.iter().peekable();
    let block = 256;
    let mut frame = 0;
    while frame < total {
        let beat = frame as f32 / frames_per_beat;
        while let Some(&&(t, ch, note, velocity)) = ev.peek() {
            if t > beat {
                break;
            }
            let kind = if velocity > 0 {
                MidiKind::NoteOn { note, velocity }
            } else {
                MidiKind::NoteOff { note }
            };
            let _ = commands.push(Command::Midi(MidiMsg { channel: ch, kind }));
            ev.next();
        }
        let n = block.min(total - frame);
        engine.process(&mut out[frame * 2..(frame + n) * 2], 2);
        frame += n;
    }

    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sr as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    for s in &out {
        w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    w.finalize()?;
    println!(
        "wrote {} ({:.1}s, peak {:.2})",
        path.display(),
        total as f32 / sr,
        peak
    );
    Ok(())
}
