//! MIDI input: parsing, hardware port connections and a virtual input port
//! (macOS/Linux) that DAWs and sequencers can target directly.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use crossbeam_queue::ArrayQueue;
use midir::{Ignore, MidiInput, MidiInputConnection};

use crate::engine::{Command, CommandQueue};

pub const CLIENT_NAME: &str = "Arkeology Synth";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MidiKind {
    NoteOn { note: u8, velocity: u8 },
    NoteOff { note: u8 },
    Cc { cc: u8, value: u8 },
    /// Normalised to -1..=1.
    PitchBend(f32),
    Program(u8),
    Aftertouch(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MidiMsg {
    /// 0-based channel (0 == MIDI channel 1).
    pub channel: u8,
    pub kind: MidiKind,
}

pub fn parse(bytes: &[u8]) -> Option<MidiMsg> {
    let status = *bytes.first()?;
    if !(0x80..0xF0).contains(&status) {
        return None;
    }
    let channel = status & 0x0F;
    let d1 = bytes.get(1).copied().unwrap_or(0) & 0x7F;
    let d2 = bytes.get(2).copied().unwrap_or(0) & 0x7F;
    let kind = match status & 0xF0 {
        0x80 => MidiKind::NoteOff { note: d1 },
        0x90 if d2 == 0 => MidiKind::NoteOff { note: d1 },
        0x90 => MidiKind::NoteOn { note: d1, velocity: d2 },
        0xB0 => MidiKind::Cc { cc: d1, value: d2 },
        0xC0 => MidiKind::Program(d1),
        0xD0 => MidiKind::Aftertouch(d1),
        0xE0 => {
            let v = ((d2 as i32) << 7 | d1 as i32) - 8192;
            MidiKind::PitchBend((v as f32 / 8192.0).clamp(-1.0, 1.0))
        }
        _ => return None,
    };
    Some(MidiMsg { channel, kind })
}

const NOTE_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

pub fn note_name(note: u8) -> String {
    format!("{}{}", NOTE_NAMES[note as usize % 12], note as i32 / 12 - 1)
}

impl std::fmt::Display for MidiMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ch{:<2} ", self.channel + 1)?;
        match self.kind {
            MidiKind::NoteOn { note, velocity } => write!(f, "on  {:<4} v{velocity}", note_name(note)),
            MidiKind::NoteOff { note } => write!(f, "off {}", note_name(note)),
            MidiKind::Cc { cc, value } => write!(f, "cc{cc} = {value}"),
            MidiKind::PitchBend(v) => write!(f, "bend {v:+.2}"),
            MidiKind::Program(p) => write!(f, "program {}", p + 1),
            MidiKind::Aftertouch(v) => write!(f, "pressure {v}"),
        }
    }
}

/// Where incoming MIDI goes: straight to the audio engine, and a copy to the
/// UI for the monitor, MIDI learn and CC mappings.
#[derive(Clone)]
pub struct Sink {
    pub engine: CommandQueue,
    pub ui: Arc<ArrayQueue<MidiMsg>>,
}

impl Sink {
    fn deliver(&self, bytes: &[u8]) {
        if let Some(msg) = parse(bytes) {
            let _ = self.engine.push(Command::Midi(msg));
            let _ = self.ui.push(msg);
        }
    }
}

pub struct MidiManager {
    sink: Sink,
    connections: Vec<(String, MidiInputConnection<()>)>,
    virtual_port: Option<MidiInputConnection<()>>,
}

fn new_input() -> Result<MidiInput> {
    let mut input = MidiInput::new(CLIENT_NAME)?;
    input.ignore(Ignore::SysexAndTime | Ignore::ActiveSense);
    Ok(input)
}

impl MidiManager {
    pub fn new(sink: Sink) -> Self {
        MidiManager {
            sink,
            connections: Vec::new(),
            virtual_port: None,
        }
    }

    pub fn available_ports() -> Vec<String> {
        let Ok(input) = new_input() else { return Vec::new() };
        input
            .ports()
            .iter()
            .filter_map(|p| input.port_name(p).ok())
            // Don't loop our own virtual port back into ourselves.
            .filter(|name| !name.contains(CLIENT_NAME))
            .collect()
    }

    pub fn is_connected(&self, name: &str) -> bool {
        self.connections.iter().any(|(n, _)| n == name)
    }

    pub fn connected_names(&self) -> Vec<String> {
        self.connections.iter().map(|(n, _)| n.clone()).collect()
    }

    pub fn has_virtual(&self) -> bool {
        self.virtual_port.is_some()
    }

    pub fn connect(&mut self, name: &str) -> Result<()> {
        if self.is_connected(name) {
            return Ok(());
        }
        let input = new_input()?;
        let port = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).ok().as_deref() == Some(name))
            .ok_or_else(|| anyhow!("MIDI port '{name}' not found"))?;
        let sink = self.sink.clone();
        let conn = input
            .connect(&port, "arkeology-in", move |_ts, bytes, _| sink.deliver(bytes), ())
            .map_err(|e| anyhow!("connecting to '{name}': {e}"))?;
        self.connections.push((name.to_string(), conn));
        Ok(())
    }

    pub fn disconnect(&mut self, name: &str) {
        if let Some(pos) = self.connections.iter().position(|(n, _)| n == name) {
            let (_, conn) = self.connections.remove(pos);
            conn.close();
        }
    }

    /// Connect every available hardware/software port. Returns how many succeeded.
    pub fn connect_all(&mut self) -> usize {
        Self::available_ports()
            .iter()
            .filter(|name| self.connect(name).is_ok())
            .count()
    }

    #[cfg(unix)]
    pub fn open_virtual(&mut self) -> Result<()> {
        use midir::os::unix::VirtualInput;
        let input = new_input()?;
        let sink = self.sink.clone();
        let conn = input
            .create_virtual(CLIENT_NAME, move |_ts, bytes, _| sink.deliver(bytes), ())
            .map_err(|e| anyhow!("creating virtual port: {e}"))?;
        self.virtual_port = Some(conn);
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn open_virtual(&mut self) -> Result<()> {
        Err(anyhow!("virtual MIDI ports are not supported on this platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_messages() {
        assert_eq!(
            parse(&[0x91, 60, 100]),
            Some(MidiMsg { channel: 1, kind: MidiKind::NoteOn { note: 60, velocity: 100 } })
        );
        assert_eq!(
            parse(&[0x90, 60, 0]),
            Some(MidiMsg { channel: 0, kind: MidiKind::NoteOff { note: 60 } })
        );
        let Some(MidiMsg { kind: MidiKind::PitchBend(v), .. }) = parse(&[0xE0, 0, 64]) else {
            panic!()
        };
        assert!(v.abs() < 1e-6);
        assert_eq!(parse(&[0xF8]), None);
    }
}
