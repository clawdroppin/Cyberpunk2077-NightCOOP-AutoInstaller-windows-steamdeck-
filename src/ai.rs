//! Optional AI helper. Talks to a model the player runs themselves (Ollama, LM Studio, llama.cpp's
//! server, Jan…) through the OpenAI-compatible HTTP API they all offer. Nothing is sent anywhere
//! unless the player sets it up, and only to the address they choose.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_URL: &str = "http://localhost:11434/v1";

/// Local servers tried by "Find my AI": Ollama, LM Studio, llama.cpp, Jan.
const KNOWN_SERVERS: [(&str, &str); 4] = [
    ("Ollama", "http://localhost:11434/v1"),
    ("LM Studio", "http://localhost:1234/v1"),
    ("llama.cpp", "http://localhost:8080/v1"),
    ("Jan", "http://localhost:1337/v1"),
];

#[derive(Clone)]
pub struct Message {
    /// "user" or "assistant"
    pub role: &'static str,
    pub content: String,
}

pub enum Event {
    Delta(String),
    /// The AI wants to use the helper. Answer with one result per call on the results channel.
    Tools(Vec<ToolCall>),
    /// Something the player should know that isn't part of the answer.
    Notice(String),
    Done,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

impl ToolCall {
    pub fn arg(&self, key: &str) -> String {
        match &self.args[key] {
            Value::String(s) => s.trim().to_string(),
            Value::Null => String::new(),
            v => v.to_string(),
        }
    }
    pub fn flag(&self, key: &str) -> bool {
        matches!(&self.args[key], Value::Bool(true)) || self.arg(key).eq_ignore_ascii_case("true")
    }
}

/// How much a tool may do: looking is free, changes need the player's OK (unless they allowed
/// the AI to act on its own), and the destructive ones always ask.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Look,
    Change,
    Always,
}

/// (name, what it does, parameters as (name, type, description, required), risk)
type ToolDef = (&'static str, &'static str, &'static [(&'static str, &'static str, &'static str, bool)], Risk);

pub const TOOLS: &[ToolDef] = &[
    ("get_status", "Everything the helper knows right now: game, RED4ext, NightCoop, Proton launch option, Visual C++ runtime, saves, mods on/off, available updates. Call this first.", &[], Risk::Look),
    ("read_logs", "The latest RED4ext and NightCoop log lines (why a plugin didn't load, connection errors).", &[], Risk::Look),
    ("check_for_updates", "Looks up the newest NightCoop, RED4ext, helper and hub versions online.", &[], Risk::Look),
    ("list_nightcoop_builds", "The NightCoop builds that can be installed, with protocol numbers and supported game versions.", &[], Risk::Look),
    ("list_save_backups", "The save backups the helper made, newest first.", &[], Risk::Look),
    ("check_connection", "Tests whether the NightCoop server can be found and UDP gets out; reports the router type.", &[], Risk::Look),
    ("compare_with_partner", "Compares this player's versions with what their partner sent, and decodes an invite in it.", &[("text", "string", "what the partner sent", true)], Risk::Look),
    ("open_page", "Shows a page of the helper to the player.", &[("page", "string", "home, together, versions, saves, server, troubleshoot, guide or settings", true)], Risk::Look),
    ("get_server_status", "The player's own hub on this computer: built, running, firewall, fingerprint, updates.", &[], Risk::Look),
    ("check_router", "Asks the router (UPnP) for this computer's address and the public address; detects CGNAT.", &[], Risk::Look),
    ("install_or_update_everything", "The main button: installs or updates NightCoop with its RED4ext, the Visual C++ runtime (Proton) and the Steam launch option, backing up saves first.", &[("restart_steam", "boolean", "allow closing and restarting Steam to set the launch option (not possible in Gaming Mode)", false)], Risk::Change),
    ("install_nightcoop_version", "Installs one specific NightCoop build (e.g. to match the partner).", &[("version", "string", "a version from list_nightcoop_builds", true)], Risk::Change),
    ("mods_off", "Turns all mods off so the game starts unmodded; everything is kept.", &[], Risk::Change),
    ("mods_on", "Turns the mods back on.", &[], Risk::Change),
    ("backup_saves", "Makes a backup of the game's saves now.", &[], Risk::Change),
    ("install_vc_runtime", "Installs Microsoft's Visual C++ runtime into the game's Proton prefix (fixes RED4ext error 998 on Steam Deck/Linux).", &[], Risk::Change),
    ("set_steam_launch_option", "Adds or removes the Proton launch option. Closes and restarts Steam (Desktop Mode only).", &[("enable", "boolean", "true to add, false to remove", true)], Risk::Change),
    ("use_bundled_red4ext", "Switches back to the RED4ext that comes with NightCoop (recommended).", &[], Risk::Change),
    ("set_update_channel", "Chooses stable or beta NightCoop builds.", &[("channel", "string", "stable or beta", true)], Risk::Change),
    ("launch_game", "Starts Cyberpunk 2077 through Steam.", &[], Risk::Change),
    ("add_helper_to_steam", "Adds this helper to the Steam library so it opens from Gaming Mode (restarts Steam).", &[], Risk::Change),
    ("update_helper", "Updates this helper app to its newest version.", &[], Risk::Change),
    ("build_server", "Builds or updates the player's own hub on this computer from the official source (downloads a compiler once).", &[], Risk::Change),
    ("start_server", "Starts the player's own hub on this computer.", &[], Risk::Change),
    ("stop_server", "Stops the player's own hub.", &[], Risk::Change),
    ("open_server_firewall", "Windows only: lets the hub's UDP ports through Windows Firewall (one admin prompt).", &[], Risk::Change),
    ("open_router_ports", "Asks the router (UPnP) to forward or close the hub's ports to this computer.", &[("open", "boolean", "true to forward, false to close", true)], Risk::Change),
    ("set_server_autostart", "Starts the hub whenever this computer starts.", &[("on", "boolean", "true or false", true)], Risk::Change),
    ("use_server_in_game", "Points the game at a server: address#FINGERPRINT.", &[("address", "string", "like 203.0.113.7#K3QF-9X2M-TV7A-0D4R", true)], Risk::Change),
    ("uninstall_nightcoop", "Removes NightCoop (saves stay).", &[], Risk::Always),
    ("restore_save_backup", "Replaces the current saves with a backup (the current saves are backed up first).", &[("name", "string", "a name from list_save_backups", true)], Risk::Always),
    ("remove_everything", "Uninstalls NightCoop, RED4ext and the launch option: a stock game again.", &[], Risk::Always),
];

pub fn risk(name: &str) -> Risk {
    TOOLS.iter().find(|t| t.0 == name).map(|t| t.3).unwrap_or(Risk::Always)
}

fn tool_specs() -> Value {
    Value::Array(
        TOOLS
            .iter()
            .map(|(name, desc, params, _)| {
                let props: serde_json::Map<String, Value> = params.iter().map(|(p, ty, d, _)| (p.to_string(), json!({ "type": ty, "description": d }))).collect();
                let required: Vec<&str> = params.iter().filter(|p| p.3).map(|p| p.0).collect();
                json!({ "type": "function", "function": { "name": name, "description": desc,
                    "parameters": { "type": "object", "properties": props, "required": required } } })
            })
            .collect(),
    )
}

/// What the helper knows, so even a small model answers about *this* app and mod, not in general.
const KNOWLEDGE: &str = "\
FACTS ABOUT NIGHTCOOP AND THIS HELPER
- NightCoop (nightcoop.net) is a co-op mod for Cyberpunk 2077. It is a RED4ext plugin: red4ext/plugins/NightCoop/NightCoop.dll. \
It needs no Cyber Engine Tweaks. Each NightCoop build ships the RED4ext it was tested with (bin/x64/winmm.dll + red4ext/RED4ext.dll).
- When it works, the game's main menu has a \"Co-op\" item. The host picks Co-op > Host a session and copies the invite; \
the guest picks Co-op > Join by code and pastes it. Invites look like CODE@server#FINGERPRINT or just a 6-letter code.
- Everyone in a session needs builds with the same protocol number (usually the same NightCoop version). \
Phantom Liberty saves don't load in a session hosted without the expansion. Guests' own saves are never changed.
- Steam Deck / Linux runs the game through Proton and needs two extra things, both done by the helper's \"Install everything\" button: \
the Steam launch option WINEDLLOVERRIDES=\"winmm,version=n,b\" %command% and Microsoft's Visual C++ 2015-2022 runtime inside the game's Proton prefix. \
Without the runtime RED4ext shows \"RED4ext (error 998)\" / \"RED4ext could not be loaded\". Manual fix: Protontricks, \
flatpak run com.github.Matoking.protontricks 1091500 --force vcrun2022. The Proton prefix only exists after the game was started once.
- Steam rewrites its settings when it exits, so launch options can only be changed while Steam is closed. The helper restarts Steam for that \
in Desktop Mode; in Gaming Mode it can't (that would end the Deck's session) — switch to Desktop Mode or paste the line in Cyberpunk > Properties > Launch Options.
- Game updates often break RED4ext and plugins until they're updated. NightCoop lists the game versions each build supports.
- The game's UDP traffic goes through NightCoop's hub (hub.nightcoop.net, UDP 47800) and STUN; strict routers fall back to the relay. \
School/work Wi-Fi often blocks UDP. Players can host their own hub (UDP 47800 and 47810+) on a Linux VPS, a Linux PC, or Windows with WSL2.
- Logs: red4ext/logs/ (RED4ext's own log says why a plugin was skipped) and red4ext/plugins/NightCoop/logs/.

THE HELPER'S PAGES
- Home: status checks and the big yellow \"Install everything / Update\" button (installs or updates NightCoop with its RED4ext, \
the Visual C++ runtime and the launch option, backs up saves first), Play, Check for updates, Mods off (play unmodded; one click turns them back on).
- Play together: compare versions with the partner (\"Copy my versions\"), switch to their build, decode invites, connection check, Deck paste tip \
(map a back button to Ctrl+V: controller layout > L4 > Keyboard V plus a second command Left Ctrl), servers.txt editor.
- Mods & Versions: pick any NightCoop build, roll back, install from file, RED4ext choice (bundled = recommended, or my own version), \
Remove everything, launch options.
- Saves: backups, restore, export, an extra copy folder, Steam Cloud info (Steam syncs on game exit; there is no button to force it).
- Host a server: builds the official hub on this computer (Windows or Linux) with one button, firewall, start/stop, start with the computer, \
automatic updates from the official source; a one-command VPS setup; router check and UPnP port forwarding; \"Use this server in my game\".
- Troubleshoot: diagnostics, bug report zip (send it to the NightCoop Discord), Visual C++ runtime button, Verify game files, common problems.
- Deck Guide: Desktop Mode for setup, Add to Steam so the helper opens from Gaming Mode.";

const AGENT_RULES: &str = "\n\nYOU CAN USE THE HELPER\n\
You have tools that look at and operate this helper, so you can fix things for the player instead of only explaining. Rules:\n\
1. To diagnose, call get_status first (and read_logs for loading problems) instead of guessing.\n\
2. Say in one sentence what you are about to do, then call the tool. Tools that change something are shown to the player, \
who may approve or decline; a declined tool is not an error — ask what they prefer.\n\
3. Only say something worked after its result says so. If a result says FAILED, explain it simply and suggest the next step.\n\
4. Never remove mods, uninstall, or restore saves unless the player clearly asked for that.\n\
5. Don't call the same tool again and again; after a few steps, summarise for the player.
6. Never say something is up to date unless an update check says so; if updates weren't checked, call check_for_updates.";

fn system_prompt(diagnostics: Option<&str>, tools: bool) -> String {
    let mut p = format!(
        "You are the friendly assistant built into NightCoop Helper, an app that installs and troubleshoots the NightCoop co-op mod \
for Cyberpunk 2077. Most users play on a Steam Deck and are not technical. Answer in plain language with short numbered steps, \
and prefer the helper's own buttons (name the page and button) over manual work. Keep answers under 200 words unless asked for more. \
Use Markdown sparingly; put commands in fenced code blocks. If you are not sure, say so and suggest Troubleshoot > Create bug report \
and the NightCoop Discord. Never invent buttons, versions or settings that aren't described below.\n\n{KNOWLEDGE}"
    );
    if tools {
        p.push_str(AGENT_RULES);
    }
    if let Some(d) = diagnostics {
        p.push_str("\n\nTHIS PLAYER'S CURRENT SETUP (from the helper's diagnostics; use it to pinpoint the problem):\n");
        p.push_str(d);
    }
    p
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        // Big models on modest hardware can think for a while before the first word.
        .timeout_per_call(None)
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .timeout_recv_body(None)
        .http_status_as_error(false)
        .build()
        .into()
}

fn endpoint(base_url: &str, path: &str) -> String {
    format!("{}/{path}", base_url.trim().trim_end_matches('/'))
}

fn friendly_connect_error(e: &ureq::Error, base_url: &str) -> String {
    match e {
        ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => format!(
            "Couldn't reach an AI at {}. Is Ollama (or LM Studio's server) running? On another computer, it must accept connections from the network.",
            base_url.trim()
        ),
        ureq::Error::Timeout(_) => "The AI took too long to answer.".to_string(),
        other => format!("Connection problem: {other}"),
    }
}

fn with_key(req: ureq::RequestBuilder<ureq::typestate::WithoutBody>, key: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    if key.trim().is_empty() { req } else { req.header("Authorization", format!("Bearer {}", key.trim())) }
}

/// Asks the server which models it offers. Blocking: call it from a background thread.
pub fn list_models(base_url: &str, key: &str) -> Result<Vec<String>, String> {
    let mut resp = with_key(agent().get(&endpoint(base_url, "models")), key).call().map_err(|e| friendly_connect_error(&e, base_url))?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("The AI server answered with status {status}: {}", shorten(&body, 200)));
    }
    let value: Value = serde_json::from_str(&body).map_err(|_| "That address answered, but not like an AI server. Check it ends in /v1.".to_string())?;
    let models: Vec<String> = value["data"]
        .as_array()
        .map(|list| list.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    // Embedding models can't chat; put them last.
    let (mut chat, embed): (Vec<String>, Vec<String>) = models.into_iter().partition(|m| !m.contains("embed"));
    chat.extend(embed);
    Ok(chat)
}

/// Looks for an AI server on this computer: (name, address, models).
pub fn find_local() -> Option<(&'static str, &'static str, Vec<String>)> {
    std::thread::scope(|sc| {
        let probes: Vec<_> = KNOWN_SERVERS.iter().map(|&(name, url)| sc.spawn(move || list_models(url, "").ok().map(|m| (name, url, m)))).collect();
        probes.into_iter().filter_map(|h| h.join().ok().flatten()).next()
    })
}

/// Sends the conversation and streams the answer back on `tx`, until done or `stop` is set. With
/// `results`, the AI may use the helper's tools: each request arrives as [`Event::Tools`] and waits
/// for one `(id, result)` per call. Blocking: call it from a background thread.
#[allow(clippy::too_many_arguments)]
pub fn chat(
    base_url: String,
    key: String,
    mut model: String,
    diagnostics: Option<String>,
    history: Vec<Message>,
    results: Option<Receiver<Vec<(String, String)>>>,
    stop: Arc<AtomicBool>,
    tx: Sender<Event>,
    repaint: impl Fn(),
) {
    let send = |event: Event| {
        let _ = tx.send(event);
        repaint();
    };
    let mut use_tools = results.is_some();
    let mut messages = vec![json!({ "role": "system", "content": system_prompt(diagnostics.as_deref(), use_tools) })];
    // Keep within small models' context: only the last ten turns.
    let turns: Vec<&Message> = history.iter().filter(|m| m.role != "tool").collect();
    for m in turns.iter().skip(turns.len().saturating_sub(10)) {
        messages.push(json!({ "role": m.role, "content": m.content }));
    }
    if model.trim().is_empty() {
        match list_models(&base_url, &key) {
            Ok(list) if !list.is_empty() => model = list[0].clone(),
            Ok(_) => return send(Event::Error("The AI server has no model yet. Run `ollama pull qwen2.5:7b` (or download one in LM Studio) first.".into())),
            Err(e) => return send(Event::Error(e)),
        }
    }
    // A round is one answer from the model; tool results start the next one.
    for _round in 0..12 {
        let mut body = json!({ "model": model.trim(), "messages": messages, "stream": true, "temperature": 0.3 });
        if use_tools {
            body["tools"] = tool_specs();
        }
        let mut req = agent().post(&endpoint(&base_url, "chat/completions")).header("Content-Type", "application/json");
        if !key.trim().is_empty() {
            req = req.header("Authorization", format!("Bearer {}", key.trim()));
        }
        let resp = match req.send(body.to_string().as_bytes()) {
            Ok(r) => r,
            Err(e) => return send(Event::Error(friendly_connect_error(&e, &base_url))),
        };
        let status = resp.status().as_u16();
        let mut body = resp.into_body();
        if status != 200 {
            let text = body.read_to_string().unwrap_or_default();
            if use_tools && status == 400 && text.to_ascii_lowercase().contains("tool") {
                use_tools = false;
                messages[0] = json!({ "role": "system", "content": system_prompt(diagnostics.as_deref(), false) });
                send(Event::Notice(format!(
                    "The model “{}” can't use the helper's buttons, so it can only explain. For hands-on help pick a model with tool support (qwen2.5, llama3.1, mistral…).",
                    model.trim()
                )));
                continue;
            }
            return send(Event::Error(format!("The AI server answered with status {status}: {}", shorten(&text, 300))));
        }

        let mut text = String::new();
        // Tool calls arrive in pieces, keyed by index: (id, name, arguments).
        let mut calls: Vec<(String, String, String)> = Vec::new();
        let reader = BufReader::new(body.into_reader());
        for line in reader.lines() {
            if stop.load(Ordering::Relaxed) {
                return send(Event::Done);
            }
            let Ok(line) = line else {
                return send(Event::Error("The connection to the AI was lost.".into()));
            };
            let data = line.trim().strip_prefix("data:").unwrap_or(line.trim()).trim();
            if data.is_empty() || data.starts_with(':') {
                continue;
            }
            if data == "[DONE]" {
                break;
            }
            let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(err) = value.get("error") {
                let msg = err["message"].as_str().map(String::from).unwrap_or_else(|| err.to_string());
                return send(Event::Error(msg));
            }
            let choice = &value["choices"][0];
            // Streaming chunks carry `delta`; a server that ignores `stream` sends one `message`.
            let part = if choice["delta"].is_object() { &choice["delta"] } else { &choice["message"] };
            if let Some(t) = part["content"].as_str().filter(|t| !t.is_empty()) {
                text.push_str(t);
                send(Event::Delta(t.to_string()));
            }
            for (n, tc) in part["tool_calls"].as_array().into_iter().flatten().enumerate() {
                let idx = tc["index"].as_u64().map(|i| i as usize).unwrap_or(n);
                while calls.len() <= idx {
                    calls.push(Default::default());
                }
                let c = &mut calls[idx];
                if let Some(id) = tc["id"].as_str() {
                    c.0 = id.to_string();
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    c.1.push_str(name);
                }
                match &tc["function"]["arguments"] {
                    Value::String(a) => c.2.push_str(a),
                    Value::Null => {}
                    other => c.2.push_str(&other.to_string()),
                }
            }
        }
        let calls: Vec<ToolCall> = calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.1.is_empty())
            .map(|(i, (id, name, args))| ToolCall {
                id: if id.is_empty() { format!("call_{i}") } else { id },
                name,
                args: serde_json::from_str(&args).unwrap_or(Value::Null),
            })
            .collect();
        let Some(results) = results.as_ref().filter(|_| !calls.is_empty()) else {
            return send(Event::Done);
        };
        messages.push(json!({
            "role": "assistant",
            "content": text,
            "tool_calls": calls.iter().map(|c| json!({ "id": c.id, "type": "function",
                "function": { "name": c.name, "arguments": c.args.to_string() } })).collect::<Vec<_>>(),
        }));
        send(Event::Tools(calls));
        // Wait for the helper to run them (approvals can take a while).
        let answers = loop {
            if stop.load(Ordering::Relaxed) {
                return send(Event::Done);
            }
            match results.recv_timeout(Duration::from_millis(250)) {
                Ok(a) => break a,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return send(Event::Done),
            }
        };
        for (id, result) in answers {
            messages.push(json!({ "role": "tool", "tool_call_id": id, "content": result }));
        }
        // Keep the answers of separate rounds apart in the chat.
        send(Event::Delta("\n\n".into()));
    }
    send(Event::Notice("The AI took many steps in a row, so it was paused. Ask it to continue if needed.".into()));
    send(Event::Done);
}

pub fn shorten(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_endpoints_and_prompt() {
        assert_eq!(endpoint(" http://localhost:1234/v1/ ", "models"), "http://localhost:1234/v1/models");
        assert_eq!(shorten("abcdef", 3), "abc…");
        let p = system_prompt(Some("NightCoop: 0.1.1-beta.2"), false);
        assert!(p.contains("error 998") && p.ends_with("NightCoop: 0.1.1-beta.2"));
        assert!(system_prompt(None, true).contains("get_status"));
        let specs = tool_specs();
        assert_eq!(specs.as_array().unwrap().len(), TOOLS.len());
        assert_eq!(specs[1]["function"]["name"], "read_logs");
        assert!(risk("remove_everything") == Risk::Always && risk("get_status") == Risk::Look && risk("nonsense") == Risk::Always);
    }

    /// Needs a running server: NCH_AI_URL=http://localhost:11434/v1 cargo test live_chat -- --ignored
    #[test]
    #[ignore]
    fn live_chat() {
        let url = std::env::var("NCH_AI_URL").unwrap_or_else(|_| DEFAULT_URL.into());
        let models = list_models(&url, "").unwrap();
        assert!(!models.is_empty());
        let (tx, rx) = std::sync::mpsc::channel();
        let question = vec![Message { role: "user", content: "In one sentence: what fixes RED4ext error 998 on a Steam Deck?".into() }];
        chat(url, String::new(), String::new(), None, question, None, Arc::new(AtomicBool::new(false)), tx, || {});
        let mut answer = String::new();
        for ev in rx {
            match ev {
                Event::Delta(t) => answer.push_str(&t),
                Event::Done => break,
                Event::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        println!("{answer}");
        assert!(!answer.trim().is_empty());
    }

    /// The model asks for get_status, gets a (made-up) setup back, and explains it.
    #[test]
    #[ignore]
    fn live_tools() {
        let url = std::env::var("NCH_AI_URL").unwrap_or_else(|_| DEFAULT_URL.into());
        let (tx, rx) = std::sync::mpsc::channel();
        let (rtx, rrx) = std::sync::mpsc::channel();
        let question = vec![Message { role: "user", content: "There's no Co-op menu in my game. Check my setup and tell me what's wrong.".into() }];
        let worker = std::thread::spawn(move || chat(url, String::new(), String::new(), None, question, Some(rrx), Arc::new(AtomicBool::new(false)), tx, || {}));
        let (mut answer, mut used) = (String::new(), Vec::new());
        for ev in rx {
            match ev {
                Event::Delta(t) => answer.push_str(&t),
                Event::Tools(calls) => {
                    let replies = calls
                        .iter()
                        .map(|c| {
                            used.push(c.name.clone());
                            let r = if c.name == "get_status" {
                                "Platform: Steam Deck\nRED4ext: 1.30.0\nNightCoop: 0.1.1-beta.2\nProton DLL override: set\nVisual C++ runtime: MISSING (RED4ext error 998)\nReady to play: false"
                            } else {
                                "RED4ext log: error 998 loading RED4ext.dll"
                            };
                            (c.id.clone(), r.to_string())
                        })
                        .collect();
                    rtx.send(replies).unwrap();
                }
                Event::Notice(n) => println!("notice: {n}"),
                Event::Done => break,
                Event::Error(e) => panic!("{e}"),
            }
        }
        worker.join().unwrap();
        println!("tools used: {used:?}\n{answer}");
        assert!(used.iter().any(|t| t == "get_status"), "the model should look before answering");
    }
}
