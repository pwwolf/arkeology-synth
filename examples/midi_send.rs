//! Send a short test phrase to the synth's virtual MIDI port.
//!
//!     cargo run --example midi_send            # channels 1 and 2
//!     cargo run --example midi_send -- 3       # only channel 3
//!
//! Handy for checking routing without a keyboard attached.

use std::thread::sleep;
use std::time::Duration;

use midir::MidiOutput;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let channels: Vec<u8> = match std::env::args().nth(1) {
        Some(ch) => vec![ch.parse::<u8>()?.clamp(1, 16) - 1],
        None => vec![0, 1],
    };
    let out = MidiOutput::new("arkeology-midi-send")?;
    let port = out
        .ports()
        .into_iter()
        .find(|p| out.port_name(p).is_ok_and(|n| n.contains("Arkeology Synth")))
        .ok_or("the 'Arkeology Synth' virtual port isn't open; start the synth first")?;
    let mut conn = out.connect(&port, "arkeology-midi-send")?;
    for &ch in &channels {
        println!("channel {}", ch + 1);
        for (note, vel) in [(60u8, 100u8), (64, 90), (67, 80), (72, 100)] {
            conn.send(&[0x90 | ch, note, vel])?;
            sleep(Duration::from_millis(250));
        }
        conn.send(&[0xB0 | ch, 74, 64])?;
        sleep(Duration::from_millis(600));
        for note in [60u8, 64, 67, 72] {
            conn.send(&[0x80 | ch, note, 0])?;
        }
        sleep(Duration::from_millis(400));
    }
    Ok(())
}
