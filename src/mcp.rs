//! Embedded MCP server (Streamable HTTP transport) so an MCP client such as
//! Claude Code can inspect and configure the running synth.
//!
//! The server listens on 127.0.0.1 only and answers each JSON-RPC request
//! with a plain JSON response (no server-initiated streams). Tool calls are
//! forwarded to the UI thread, which owns the application state, and run
//! through the same code paths as keyboard edits.

use std::io::Read;
use std::net::SocketAddr;
use std::sync::mpsc::{Sender, SyncSender, sync_channel};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

pub const DEFAULT_PORT: u16 = 7878;
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const MAX_BODY: u64 = 1 << 20;
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// A tool call handed to the UI thread; the result is sent back on `reply`.
pub struct Job {
    pub tool: String,
    pub args: Value,
    pub reply: SyncSender<Result<Value, String>>,
}

/// Start the server on its own thread. Returns the bound address.
pub fn start(port: u16, jobs: Sender<Job>) -> Result<SocketAddr> {
    let server =
        Server::http(("127.0.0.1", port)).map_err(|e| anyhow!("MCP server on port {port}: {e}"))?;
    let addr = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("MCP server has no IP address"))?;
    std::thread::Builder::new()
        .name("mcp".into())
        .spawn(move || {
            for request in server.incoming_requests() {
                handle_http(request, &jobs);
            }
        })?;
    Ok(addr)
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

/// Browsers send Origin; only allow pages served from this machine, so a
/// website can't drive the synth via DNS rebinding.
fn origin_allowed(request: &Request) -> bool {
    let Some(origin) = request.headers().iter().find(|h| h.field.equiv("Origin")) else {
        return true;
    };
    let o = origin.value.as_str();
    let host = o.split("://").nth(1).unwrap_or("");
    let host = host.split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

fn handle_http(mut request: Request, jobs: &Sender<Job>) {
    let path = request.url().split('?').next().unwrap_or("").to_string();
    let json_type = header("Content-Type", "application/json");
    let response = if path != "/mcp" {
        Response::from_string("not found; the MCP endpoint is /mcp").with_status_code(404)
    } else if !origin_allowed(&request) {
        Response::from_string("forbidden origin").with_status_code(403)
    } else if *request.method() != Method::Post {
        // No server-initiated SSE stream and no sessions to delete.
        Response::from_string("method not allowed")
            .with_status_code(405)
            .with_header(header("Allow", "POST"))
    } else {
        let mut body = String::new();
        let read = request.as_reader().take(MAX_BODY).read_to_string(&mut body);
        match read
            .ok()
            .and_then(|_| serde_json::from_str::<Value>(&body).ok())
        {
            None => {
                Response::from_string(rpc_error(Value::Null, -32700, "parse error").to_string())
                    .with_status_code(400)
                    .with_header(json_type)
            }
            Some(msg) => match handle_message(msg, jobs) {
                Some(reply) => Response::from_string(reply.to_string()).with_header(json_type),
                // Notifications and responses are acknowledged with no body.
                None => Response::from_string("").with_status_code(202),
            },
        }
    };
    let _ = request.respond(response);
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Handle one JSON-RPC message. Returns `None` for notifications.
pub fn handle_message(msg: Value, jobs: &Sender<Job>) -> Option<Value> {
    if msg.is_array() {
        return Some(rpc_error(
            Value::Null,
            -32600,
            "batch requests are not supported",
        ));
    }
    let id = msg.get("id").cloned()?;
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return None; // a response to us; we never send requests
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|v| **v == requested)
                .unwrap_or(&PROTOCOL_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "arkeology-synth", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_definitions() }),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_error(id, -32602, "tools/call needs a tool name"));
            };
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let outcome = call_tool(name, args, jobs);
            let (text, is_error) = match outcome {
                Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
                Err(e) => (e, true),
            };
            json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
        }
        _ => {
            return Some(rpc_error(
                id,
                -32601,
                &format!("method not found: {method}"),
            ));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn call_tool(name: &str, args: Value, jobs: &Sender<Job>) -> Result<Value, String> {
    let (tx, rx) = sync_channel(1);
    jobs.send(Job {
        tool: name.to_string(),
        args,
        reply: tx,
    })
    .map_err(|_| "the synth is shutting down".to_string())?;
    rx.recv_timeout(TOOL_TIMEOUT)
        .map_err(|_| "the synth didn't answer in time".to_string())?
}

const INSTRUCTIONS: &str = "Controls the Arkeology Synth running in the user's terminal. \
The rack has up to 16 slots (numbered 1-16); each holds one synth (fm, analog, physical, granular, acid, drums, kit or sampler) \
listening on a MIDI channel (1-16 or \"omni\"). Use \"master\" as the slot for the master bus. \
Call get_rack first, and get_params to see a slot's parameter keys, ranges, defaults, current values, \
mapped MIDI CCs and how the instrument responds to MIDI (mod wheel, velocity, sustain, bend). \
describe_synth gives any synth type's full control reference without touching the rack. \
map_cc / list_midi_mappings / clear_midi_mapping manage which controller knobs drive which parameters. \
set_params accepts numbers in the parameter's own units or strings such as \"250ms\", \"2.5k\", \"40%\", \"c4\" \
or an option name like \"LowPass\". Changes apply live and show in the user's TUI. \
play_notes auditions sounds through the user's speakers.";

fn slot_schema() -> Value {
    json!({ "type": "integer", "minimum": 1, "maximum": 16, "description": "Slot number (1-16)" })
}

fn tool_definitions() -> Value {
    let slot = slot_schema();
    let slot_or_master = json!({
        "description": "Slot number (1-16) or \"master\"",
        "oneOf": [slot.clone(), { "type": "string", "enum": ["master"] }]
    });
    let channel = json!({
        "description": "MIDI channel 1-16, or \"omni\" for all channels",
        "oneOf": [{ "type": "integer", "minimum": 1, "maximum": 16 }, { "type": "string", "enum": ["omni"] }]
    });
    let kind = json!({
        "type": "string",
        "enum": ["fm", "analog", "physical", "tonewheel", "granular", "acid", "drums", "kit", "sampler"],
        "description": "fm: 4-op FM · analog: poly subtractive (pads, brass, leads) · \
    physical: modelled plucked strings, mallets/bells, piano and bowed strings (violin family) · granular: grain clouds · \
    acid: 303-style mono bass · drums: synthesized 808/909 kit on the GM drum map · \
    kit: sample drum kit, 16 pads with their own WAV/FLAC files on GM notes · sampler: WAV/FLAC sampler (classic/one-shot/slice)"
    });
    let read_only = json!({ "readOnlyHint": true });
    json!([
        {
            "name": "get_rack",
            "description": "Overview of the rack: every slot's synth type, patch name, MIDI channel, mute/solo, \
    active voices and sample, plus master settings and connected MIDI inputs.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "get_params",
            "description": "All active parameters of a slot (or the master bus): key, name, group, kind, unit, \
    scale (linear/log), range, default, current value and display text, options for choices, and the MIDI CC \
    mapped to it if any. For a synth slot it also describes how the instrument responds to MIDI (pitch bend, \
    mod wheel, sustain, velocity, note layout). Includes the 3 insert effects (fx1_*, fx2_*, fx3_*); \
    an effect's settings appear once its fxN_type is set (Delay, Reverb, Chorus, Flanger, Phaser, Drive, \
    Filter, EQ, Compressor, Crusher, Tremolo).",
            "inputSchema": { "type": "object", "properties": { "slot": slot_or_master }, "required": ["slot"] },
            "annotations": read_only,
        },
        {
            "name": "describe_synth",
            "description": "Full control reference for a synth type without adding it: every parameter \
    (with kind, unit, scale, range, default and, where it only applies to some physical models or FX types, \
    an applies_when condition), how it responds to MIDI, and the insert-FX settings for each effect type.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": kind.clone(),
                    "include_fx": { "type": "boolean", "default": false, "description": "Also list every fx1-3 parameter in full" }
                },
                "required": ["kind"]
            },
            "annotations": read_only,
        },
        {
            "name": "list_midi_mappings",
            "description": "Controller mappings: which MIDI channel + CC drives which parameter (slot or master), \
    and whether that parameter is currently active.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "map_cc",
            "description": "Map a MIDI CC (on one channel) to a parameter, like MIDI learn. The CC sweeps the \
    parameter's full range (log-scaled parameters sweep logarithmically; choices step through options). A \
    parameter has at most one CC; one CC can drive several parameters (macros). CC1 and CC64 keep their \
    built-in roles; CC120-127 can't be mapped. Saved with the session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "integer", "minimum": 1, "maximum": 16 },
                    "cc": { "type": "integer", "minimum": 0, "maximum": 119 },
                    "slot": slot_or_master.clone(),
                    "param": { "type": "string", "description": "Parameter key from get_params, e.g. \"cutoff\"" }
                },
                "required": ["channel", "cc", "slot", "param"]
            },
        },
        {
            "name": "clear_midi_mapping",
            "description": "Remove mappings: either for one parameter (slot + param) or everything on a CC (channel + cc).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": slot_or_master.clone(),
                    "param": { "type": "string" },
                    "channel": { "type": "integer", "minimum": 1, "maximum": 16 },
                    "cc": { "type": "integer", "minimum": 0, "maximum": 127 }
                }
            },
        },
        {
            "name": "set_params",
            "description": "Set one or more parameters on a slot or the master bus. Values are numbers in the \
    parameter's units, or strings like \"250ms\", \"2.5k\", \"40%\", \"c4\", \"on\" or an option name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": slot_or_master,
                    "values": {
                        "type": "object",
                        "description": "Map of parameter key to value, e.g. {\"cutoff\": \"800\", \"resonance\": 0.7}",
                        "additionalProperties": { "type": ["number", "string", "boolean"] }
                    }
                },
                "required": ["slot", "values"]
            },
        },
        {
            "name": "add_synth",
            "description": "Add a synth in the first free slot. Drums default to channel 10, others to the \
    first unused channel. Optionally load a patch by name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": kind,
                    "channel": channel,
                    "patch": { "type": "string", "description": "Patch name to load, e.g. \"FM Strings\"" }
                },
                "required": ["kind"]
            },
        },
        {
            "name": "remove_synth",
            "description": "Remove the synth in a slot.",
            "inputSchema": { "type": "object", "properties": { "slot": slot }, "required": ["slot"] },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "set_slot",
            "description": "Change a slot's MIDI channel, mute, solo, program-change reception or name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": slot,
                    "channel": channel,
                    "mute": { "type": "boolean" },
                    "solo": { "type": "boolean", "description": "While any slot is soloed, all others are silent" },
                    "program_change": { "type": "boolean", "description": "Load patches on MIDI program change (default true). False locks the slot's sound against sequencers" },
                    "name": { "type": "string" }
                },
                "required": ["slot"]
            },
        },
        {
            "name": "list_patches",
            "description": "Factory and user patches, optionally only for one synth type. `program` is the \
    patch's number (1-128) for MIDI program change: a program change on a channel loads that number \
    for each listening synth's own type (send program value = number - 1). Factory patches come first, \
    so their numbers are stable.",
            "inputSchema": { "type": "object", "properties": { "kind": kind } },
            "annotations": read_only,
        },
        {
            "name": "load_patch",
            "description": "Load a patch by name into a slot (this can change the slot's synth type). \
    User patches win over factory patches with the same name.",
            "inputSchema": {
                "type": "object",
                "properties": { "slot": slot, "name": { "type": "string" } },
                "required": ["slot", "name"]
            },
        },
        {
            "name": "save_patch",
            "description": "Save a slot's current sound as a user patch (overwrites a user patch with the same name).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": slot,
                    "name": { "type": "string", "description": "Defaults to the slot's current name" }
                },
                "required": ["slot"]
            },
        },
        {
            "name": "new_rack",
            "description": "Replace the whole rack with an empty one or the starter rack (E.Piano, Choir Cloud, \
    Acid Classic, 808 Kit). Resets master settings and MIDI mappings; MIDI ports and any recording carry on. \
    Pass save_as to keep the current rack as a session first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "template": { "type": "string", "enum": ["empty", "starter"], "default": "empty" },
                    "save_as": { "type": "string", "description": "Session name to save the current rack under first" }
                }
            },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "list_sessions",
            "description": "Saved sessions (whole-rack snapshots).",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "save_session",
            "description": "Save the whole rack, MIDI mappings and ports as a named session.",
            "inputSchema": { "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] },
        },
        {
            "name": "load_session",
            "description": "Replace the whole rack with a saved session.",
            "inputSchema": { "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "load_sample",
            "description": "Load a WAV or FLAC file into a granular, sampler or kit slot (kits need a pad, 1-16). \
    For samplers the pitch is detected and Root Note/Tune set to match.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": slot,
                    "path": { "type": "string", "description": "Absolute path, or relative to the samples folder; ~ is expanded" },
                    "pad": { "type": "integer", "minimum": 1, "maximum": 16, "description": "Kit pad (kit slots only)" }
                },
                "required": ["slot", "path"]
            },
        },
        {
            "name": "load_kit_folder",
            "description": "Load every WAV/FLAC in a folder into a kit slot, assigning pads from file names \
    (kick, snare, hh/closed hat, open hat, clap, rim, toms, crash, ride, tambourine, cowbell, shaker); \
    unrecognised files fill the remaining pads. Returns the assignments.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": slot,
                    "path": { "type": "string", "description": "Folder path; ~ is expanded" }
                },
                "required": ["slot", "path"]
            },
        },
        {
            "name": "play_notes",
            "description": "Audition a slot by scheduling notes through the user's speakers (timing is \
    accurate to about 20 ms). Drums use GM notes: 36 kick, 38 snare, 42 closed hat, 46 open hat.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": slot,
                    "notes": {
                        "type": "array",
                        "maxItems": 512,
                        "items": {
                            "type": "object",
                            "properties": {
                                "note": { "type": ["integer", "string"], "description": "MIDI note number or name like \"c4\" (60)" },
                                "velocity": { "type": "integer", "minimum": 1, "maximum": 127, "default": 100 },
                                "start_ms": { "type": "integer", "minimum": 0, "default": 0 },
                                "duration_ms": { "type": "integer", "minimum": 1, "default": 400 }
                            },
                            "required": ["note"]
                        }
                    }
                },
                "required": ["slot", "notes"]
            },
        },
        {
            "name": "start_recording",
            "description": "Record the master output (after FX, volume and drive) to a 32-bit float stereo WAV \
    in the recordings folder. The TUI shows a REC badge while it runs.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string", "description": "File name; defaults to the date and time" } }
            },
        },
        {
            "name": "stop_recording",
            "description": "Stop recording and finalize the WAV; returns its path and length.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "panic",
            "description": "Stop all sounding and scheduled notes.",
            "inputSchema": { "type": "object", "properties": {} },
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc::channel;

    fn answer_jobs() -> Sender<Job> {
        let (tx, rx) = channel::<Job>();
        std::thread::spawn(move || {
            for job in rx {
                let r = if job.tool == "get_rack" {
                    Ok(json!({ "slots": [] }))
                } else {
                    Err("nope".into())
                };
                let _ = job.reply.send(r);
            }
        });
        tx
    }

    #[test]
    fn initialize_and_list_tools() {
        let jobs = answer_jobs();
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } });
        let r = handle_message(init, &jobs).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert!(r["result"]["capabilities"]["tools"].is_object());

        assert!(
            handle_message(
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                &jobs
            )
            .is_none()
        );

        let r = handle_message(
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            &jobs,
        )
        .unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "set_params"));
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }

        let r = handle_message(
            json!({ "jsonrpc": "2.0", "id": 3, "method": "bogus" }),
            &jobs,
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn tool_results_and_errors() {
        let jobs = answer_jobs();
        let call = |name: &str| {
            handle_message(
                json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": name, "arguments": {} } }),
                &jobs,
            )
            .unwrap()
        };
        let ok = call("get_rack");
        assert_eq!(ok["result"]["isError"], false);
        assert!(
            ok["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("slots")
        );
        let err = call("whatever");
        assert_eq!(err["result"]["isError"], true);
    }

    fn http(addr: SocketAddr, request: &str) -> String {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn serves_http_on_localhost() {
        let addr = start(0, answer_jobs()).unwrap();
        assert!(addr.ip().is_loopback());
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_rack"}}"#;
        let post = |extra: &str| {
            format!(
                "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
Accept: application/json, text/event-stream\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let ok = http(addr, &post(""));
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(
            ok.contains("application/json") && ok.contains("slots"),
            "{ok}"
        );

        let local = http(addr, &post("Origin: http://localhost:3000\r\n"));
        assert!(local.starts_with("HTTP/1.1 200"), "{local}");
        let evil = http(addr, &post("Origin: https://evil.example\r\n"));
        assert!(evil.starts_with("HTTP/1.1 403"), "{evil}");

        let get = http(
            addr,
            "GET /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(get.starts_with("HTTP/1.1 405"), "{get}");
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let notif = http(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{note}",
                note.len()
            ),
        );
        assert!(notif.starts_with("HTTP/1.1 202"), "{notif}");
    }
}
