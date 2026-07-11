//! egui front-end.
//!
//! All real-time work (receiving, mixing, presence, sending mic audio,
//! keepalives) runs on **background threads** so it keeps flowing even when the
//! window is hidden (e.g. on another Wayland/Hyprland workspace, or during a
//! resize) and the egui update loop is paused. The UI thread only renders from
//! shared state and handles user input.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use eframe::egui;

use crate::audio::{Cue, SharedMixer, Voice};
use crate::net::Net;
use crate::protocol::{Envelope, Payload};
use crate::theme::{self, Container};

struct ChatLine {
    who: String,
    body: String,
    mine: bool,
}

struct Peer {
    name: String,
    last_seen: Instant,
    speaking_until: Instant,
}

/// Log-worthy events emitted by the engine thread for the UI to append.
enum UiMsg {
    Joined(String),
    Left(String),
    Text { who: String, body: String },
    Emote { name: String, action: String },
}

type Peers = Arc<Mutex<HashMap<u64, Peer>>>;
type Levels = Arc<Mutex<HashMap<u64, f32>>>;

fn store_f32(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}
fn load_f32(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

/// Peak of a mono frame, lightly boosted so normal speech is clearly visible.
fn peak_level(samples: &[f32]) -> f32 {
    let p = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    (p * 3.0).min(1.0)
}

pub struct ChatApp {
    peer_id: u64,
    name: Arc<Mutex<String>>,
    net: Arc<Net>,
    voice: Option<Voice>,
    voice_err: Option<String>,
    /// Shared mixer for locally-originated cues (None if no audio output).
    mixer: Option<SharedMixer>,

    // Shared state updated by the engine/out threads, read by the UI.
    peers: Peers,
    peer_levels: Levels,
    mic_level: Arc<AtomicU32>,
    ui_rx: Receiver<UiMsg>,

    // UI-only state.
    draft: String,
    log: Vec<ChatLine>,
    scroll_to_bottom: bool,
    quit: bool,
    compose_focus: bool,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
}

impl ChatApp {
    pub fn new(peer_id: u64, name: String, net: Net) -> Self {
        let net = Arc::new(net);
        let name = Arc::new(Mutex::new(name));
        let peers: Peers = Arc::new(Mutex::new(HashMap::new()));
        let peer_levels: Levels = Arc::new(Mutex::new(HashMap::new()));
        let mic_level = Arc::new(AtomicU32::new(0));
        let (ui_tx, ui_rx) = crossbeam_channel::unbounded();

        let (mut voice, voice_err) = match Voice::new() {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let mixer = voice.as_ref().map(|v| v.mixer());
        let frames_rx = voice.as_mut().and_then(|v| v.take_frames_rx());

        // Engine: receive -> mix/presence/cues, forward log events to the UI.
        spawn_engine(
            net.incoming.clone(),
            peer_id,
            mixer.clone(),
            peers.clone(),
            peer_levels.clone(),
            ui_tx,
        );
        // Presence keepalive (independent of the UI loop).
        spawn_keepalive(net.clone(), name.clone(), peer_id);
        // Outbound mic audio (only if we have a capture path).
        if let Some(rx) = frames_rx {
            spawn_out(rx, net.clone(), name.clone(), peer_id, mic_level.clone());
        }

        ChatApp {
            peer_id,
            name,
            net,
            voice,
            voice_err,
            mixer,
            peers,
            peer_levels,
            mic_level,
            ui_rx,
            draft: String::new(),
            log: Vec::new(),
            scroll_to_bottom: false,
            quit: false,
            compose_focus: false,
            input_devices: crate::audio::list_input_devices(),
            output_devices: crate::audio::list_output_devices(),
        }
    }

    fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    fn play_cue(&self, cue: Cue) {
        if let Some(m) = &self.mixer {
            m.lock().unwrap().play_cue(cue);
        }
    }

    fn send_text(&mut self) {
        let body = self.draft.trim().to_string();
        if body.is_empty() {
            return;
        }
        self.draft.clear();
        if let Some(rest) = body.strip_prefix('/') {
            if !rest.starts_with('/') {
                self.handle_command(rest);
                return;
            }
            self.broadcast_text(rest.to_string());
            return;
        }
        self.broadcast_text(body);
    }

    fn broadcast_text(&mut self, body: String) {
        let who = self.name();
        let _ = self
            .net
            .send(&Envelope::new(self.peer_id, Payload::Text { name: who.clone(), body: body.clone() }));
        self.log.push(ChatLine { who, body, mine: true });
        self.scroll_to_bottom = true;
        self.play_cue(Cue::Message);
    }

    /// Local-only system line.
    fn sys(&mut self, msg: impl Into<String>) {
        self.log.push(ChatLine { who: "*".into(), body: msg.into(), mine: false });
        self.scroll_to_bottom = true;
    }

    fn handle_command(&mut self, cmd: &str) {
        let mut parts = cmd.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").to_lowercase();
        let arg = parts.next().unwrap_or("").trim().to_string();

        match name.as_str() {
            "name" | "nick" => {
                if arg.is_empty() {
                    self.sys("usage: /name <new name>");
                } else {
                    let old = {
                        let mut n = self.name.lock().unwrap();
                        std::mem::replace(&mut *n, arg.clone())
                    };
                    let _ = self
                        .net
                        .send(&Envelope::new(self.peer_id, Payload::Hello { name: arg.clone() }));
                    self.sys(format!("{old} is now known as {arg}"));
                }
            }
            "me" => {
                if arg.is_empty() {
                    self.sys("usage: /me <action>");
                } else {
                    let who = self.name();
                    let _ = self.net.send(&Envelope::new(
                        self.peer_id,
                        Payload::Emote { name: who.clone(), action: arg.clone() },
                    ));
                    self.sys(format!("{who} {arg}"));
                }
            }
            "mic" => {
                let want = match arg.to_lowercase().as_str() {
                    "on" | "1" | "true" => Some(true),
                    "off" | "0" | "false" => Some(false),
                    "" => self.voice.as_ref().map(|v| !v.mic_on()),
                    _ => None,
                };
                match (want, self.voice.as_mut()) {
                    (Some(on), Some(v)) => match v.set_mic(on) {
                        Ok(()) => self.sys(if on { "microphone on" } else { "microphone off" }),
                        Err(e) => self.sys(format!("mic error: {e}")),
                    },
                    (Some(_), None) => self.sys("voice unavailable"),
                    (None, _) => self.sys("usage: /mic [on|off]"),
                }
            }
            "peers" => {
                let mut names: Vec<String> =
                    self.peers.lock().unwrap().values().map(|p| p.name.clone()).collect();
                names.sort();
                if names.is_empty() {
                    self.sys("no other peers on the group");
                } else {
                    self.sys(format!("{} peer(s): {}", names.len(), names.join(", ")));
                }
            }
            "clear" => self.log.clear(),
            "quit" | "exit" => self.quit = true,
            "help" | "?" => {
                self.sys("commands: /name <n>, /me <action>, /mic [on|off], /peers, /clear, /quit, /help");
            }
            other => self.sys(format!("unknown command: /{other}  (try /help)")),
        }
    }
}

// ---- background threads -----------------------------------------------------

fn upsert_peer(peers: &Peers, id: u64, name: &str, now: Instant) -> bool {
    let mut p = peers.lock().unwrap();
    match p.get_mut(&id) {
        Some(pi) => {
            pi.name = name.to_string();
            pi.last_seen = now;
            false
        }
        None => {
            p.insert(id, Peer { name: name.to_string(), last_seen: now, speaking_until: now });
            true
        }
    }
}

fn spawn_engine(
    incoming: Receiver<(Envelope, SocketAddr)>,
    peer_id: u64,
    mixer: Option<SharedMixer>,
    peers: Peers,
    levels: Levels,
    ui_tx: Sender<UiMsg>,
) {
    thread::Builder::new()
        .name("engine".into())
        .spawn(move || {
            let mut last_tick = Instant::now();
            loop {
                match incoming.recv_timeout(Duration::from_millis(50)) {
                    Ok((env, _from)) => {
                        if env.peer_id == peer_id {
                            continue; // ignore our own looped-back traffic
                        }
                        let id = env.peer_id;
                        let now = Instant::now();
                        match env.payload {
                            Payload::Hello { name } => {
                                if upsert_peer(&peers, id, &name, now) {
                                    if let Some(m) = &mixer {
                                        m.lock().unwrap().play_cue(Cue::Join);
                                    }
                                    let _ = ui_tx.send(UiMsg::Joined(name));
                                }
                            }
                            Payload::Bye { name } => {
                                if peers.lock().unwrap().remove(&id).is_some() {
                                    if let Some(m) = &mixer {
                                        m.lock().unwrap().play_cue(Cue::Leave);
                                    }
                                    let _ = ui_tx.send(UiMsg::Left(name));
                                }
                            }
                            Payload::Text { name, body } => {
                                upsert_peer(&peers, id, &name, now);
                                if let Some(m) = &mixer {
                                    m.lock().unwrap().play_cue(Cue::Message);
                                }
                                let _ = ui_tx.send(UiMsg::Text { who: name, body });
                            }
                            Payload::Emote { name, action } => {
                                upsert_peer(&peers, id, &name, now);
                                let _ = ui_tx.send(UiMsg::Emote { name, action });
                            }
                            Payload::Voice { name, seq: _, pcm } => {
                                upsert_peer(&peers, id, &name, now);
                                if let Some(pi) = peers.lock().unwrap().get_mut(&id) {
                                    pi.speaking_until = now + Duration::from_millis(300);
                                }
                                let lvl = peak_level(&pcm);
                                {
                                    let mut l = levels.lock().unwrap();
                                    let e = l.entry(id).or_insert(0.0);
                                    *e = e.max(lvl);
                                }
                                if let Some(m) = &mixer {
                                    m.lock().unwrap().push_frame(id, &pcm);
                                }
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }

                // Periodic housekeeping: decay levels, expire silent peers.
                let now = Instant::now();
                if now.duration_since(last_tick) > Duration::from_millis(80) {
                    last_tick = now;
                    levels.lock().unwrap().retain(|_, v| {
                        *v *= 0.82;
                        *v > 0.01
                    });
                    peers
                        .lock()
                        .unwrap()
                        .retain(|_, p| now.duration_since(p.last_seen) < Duration::from_secs(15));
                }
            }
        })
        .expect("spawn engine");
}

fn spawn_keepalive(net: Arc<Net>, name: Arc<Mutex<String>>, peer_id: u64) {
    thread::Builder::new()
        .name("keepalive".into())
        .spawn(move || loop {
            let nm = name.lock().unwrap().clone();
            let _ = net.send(&Envelope::new(peer_id, Payload::Hello { name: nm }));
            thread::sleep(Duration::from_secs(5));
        })
        .expect("spawn keepalive");
}

fn spawn_out(
    frames_rx: Receiver<Vec<f32>>,
    net: Arc<Net>,
    name: Arc<Mutex<String>>,
    peer_id: u64,
    mic_level: Arc<AtomicU32>,
) {
    thread::Builder::new()
        .name("mic-out".into())
        .spawn(move || {
            let mut seq = 0u32;
            loop {
                match frames_rx.recv_timeout(Duration::from_millis(150)) {
                    Ok(frame) => {
                        let peak = peak_level(&frame);
                        let cur = load_f32(&mic_level);
                        store_f32(&mic_level, peak.max(cur * 0.85)); // attack + decay
                        let nm = name.lock().unwrap().clone();
                        let _ = net.send(&Envelope::new(
                            peer_id,
                            Payload::Voice { name: nm, seq, pcm: frame },
                        ));
                        seq = seq.wrapping_add(1);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        store_f32(&mic_level, load_f32(&mic_level) * 0.6);
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("spawn mic-out");
}

// ---- UI ---------------------------------------------------------------------

impl eframe::App for ChatApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain log events from the engine.
        while let Ok(msg) = self.ui_rx.try_recv() {
            match msg {
                UiMsg::Joined(name) => {
                    self.log.push(ChatLine { who: "*".into(), body: format!("{name} joined"), mine: false })
                }
                UiMsg::Left(name) => {
                    self.log.push(ChatLine { who: "*".into(), body: format!("{name} left"), mine: false })
                }
                UiMsg::Text { who, body } => self.log.push(ChatLine { who, body, mine: false }),
                UiMsg::Emote { name, action } => self.log.push(ChatLine {
                    who: "*".into(),
                    body: format!("{name} {action}"),
                    mine: false,
                }),
            }
            self.scroll_to_bottom = true;
        }

        if self.quit {
            let _ = self
                .net
                .send(&Envelope::new(self.peer_id, Payload::Bye { name: self.name() }));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let panel_frame = egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::same(12));

        egui::containers::Panel::right("sidebar")
            .default_size(268.0)
            .frame(panel_frame)
            .show(ui, |ui| {
                self.peers_card(ui);
                ui.add_space(10.0);
                self.channel_card(ui);
                ui.add_space(10.0);
                self.voice_card(ui);
            });

        egui::containers::Panel::bottom("compose")
            .frame(panel_frame)
            .show(ui, |ui| {
                self.compose_bar(ui);
            });

        egui::containers::CentralPanel::default()
            .frame(panel_frame)
            .show(ui, |ui| {
                self.chat_view(ui);
            });
    }

    fn on_exit(&mut self) {
        let _ = self
            .net
            .send(&Envelope::new(self.peer_id, Payload::Bye { name: self.name() }));
    }
}

impl ChatApp {
    fn peers_card(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let mic_on = self.voice.as_ref().map_or(false, |v| v.mic_on());
        let mic_level = load_f32(&self.mic_level);
        let myname = self.name();
        // Snapshot shared state so we don't hold locks across the UI closure.
        let mut peers: Vec<(u64, String, bool)> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| (*id, p.name.clone(), p.speaking_until > now))
            .collect();
        peers.sort_by(|a, b| a.1.cmp(&b.1));
        let levels = self.peer_levels.lock().unwrap().clone();

        Container::titled("Peers").show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::status_dot(ui, theme::ACCENT);
                ui.label(egui::RichText::new(&myname).color(theme::ACCENT).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(if mic_on { "tx" } else { "you" })
                            .font(theme::label_font())
                            .color(if mic_on { theme::ACCENT } else { theme::MUTED }),
                    );
                });
            });
            theme::vu_meter(ui, if mic_on { mic_level } else { 0.0 });
            ui.add_space(8.0);

            for (id, name, speaking) in &peers {
                ui.horizontal(|ui| {
                    let dot = if *speaking { theme::ACCENT } else { theme::BORDER };
                    theme::status_dot(ui, dot);
                    ui.label(egui::RichText::new(name).color(theme::PEER_NAME));
                    if *speaking {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new("live")
                                    .font(theme::label_font())
                                    .color(theme::ACCENT),
                            );
                        });
                    }
                });
                theme::vu_meter(ui, levels.get(id).copied().unwrap_or(0.0));
                ui.add_space(8.0);
            }
            if peers.is_empty() {
                ui.label(egui::RichText::new("waiting for peers…").italics().color(theme::MUTED));
            }
        });
    }

    fn channel_card(&mut self, ui: &mut egui::Ui) {
        let group = self.net.group();
        Container::titled("Channel").padding(12).show(ui, |ui| {
            ui.label(egui::RichText::new(format!("[{}]", group.ip())).monospace().color(theme::TEXT));
            ui.label(
                egui::RichText::new(format!("udp/{}", group.port()))
                    .font(theme::label_font())
                    .color(theme::MUTED),
            );
        });
    }

    fn voice_card(&mut self, ui: &mut egui::Ui) {
        let mut set_mic: Option<bool> = None;
        let mut new_in: Option<Option<String>> = None;
        let mut new_out: Option<Option<String>> = None;
        let mut refresh = false;
        let input_devices = &self.input_devices;
        let output_devices = &self.output_devices;
        let voice_err = self.voice_err.clone();
        let mic_level = load_f32(&self.mic_level);

        Container::titled("Voice").show(ui, |ui| {
            match &self.voice {
                Some(v) => {
                    let on = v.mic_on();
                    let label = if on { "Transmitting…" } else { "Transmit voice" };
                    let btn = egui::Button::new(
                        egui::RichText::new(label).color(if on { theme::ON_ACCENT } else { theme::TEXT }),
                    )
                    .fill(if on { theme::ACCENT } else { theme::BTN })
                    .min_size(egui::vec2(ui.available_width(), 32.0));
                    if ui.add(btn).clicked() {
                        set_mic = Some(!on);
                    }

                    if on {
                        ui.add_space(8.0);
                        theme::vu_meter(ui, mic_level);
                    }

                    // Android (AAudio) doesn't enumerate devices, and rebuilding
                    // the stream can fail; the OS handles routing. Hide the pickers.
                    if !cfg!(target_os = "android") {
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new("Input").font(theme::label_font()).color(theme::MUTED));
                    let cur_in = v.input_name().clone().unwrap_or_else(|| "Default".into());
                    egui::ComboBox::from_id_salt("input_dev")
                        .width(ui.available_width())
                        .selected_text(cur_in)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(v.input_name().is_none(), "Default").clicked() {
                                new_in = Some(None);
                            }
                            for d in input_devices {
                                if ui.selectable_label(v.input_name().as_deref() == Some(d), d).clicked() {
                                    new_in = Some(Some(d.clone()));
                                }
                            }
                        });

                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Output").font(theme::label_font()).color(theme::MUTED));
                    let cur_out = v.output_name().clone().unwrap_or_else(|| "Default".into());
                    egui::ComboBox::from_id_salt("output_dev")
                        .width(ui.available_width())
                        .selected_text(cur_out)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(v.output_name().is_none(), "Default").clicked() {
                                new_out = Some(None);
                            }
                            for d in output_devices {
                                if ui.selectable_label(v.output_name().as_deref() == Some(d), d).clicked() {
                                    new_out = Some(Some(d.clone()));
                                }
                            }
                        });

                    ui.add_space(8.0);
                    if ui
                        .add(egui::Button::new(egui::RichText::new("Rescan devices").color(theme::MUTED)))
                        .clicked()
                    {
                        refresh = true;
                    }
                    } // end !android device pickers
                }
                None => {
                    ui.label(egui::RichText::new("No audio device — chat still works.").color(theme::MUTED));
                }
            }
            if let Some(e) = &voice_err {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::from_rgb(0xff, 0x6b, 0x6b), e);
            }
        });

        if let Some(v) = &mut self.voice {
            if let Some(on) = set_mic {
                if let Err(e) = v.set_mic(on) {
                    self.voice_err = Some(e);
                }
            }
            if let Some(sel) = new_in {
                if let Err(e) = v.set_input(sel) {
                    self.voice_err = Some(e);
                }
            }
            if let Some(sel) = new_out {
                if let Err(e) = v.set_output(sel) {
                    self.voice_err = Some(e);
                }
            }
        }
        if refresh {
            self.input_devices = crate::audio::list_input_devices();
            self.output_devices = crate::audio::list_output_devices();
        }
    }

    fn compose_bar(&mut self, ui: &mut egui::Ui) {
        let mut submit = false;
        Container::new().accent(self.compose_focus).padding(4).show(ui, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let send = egui::Button::new(egui::RichText::new("Send").color(theme::ON_ACCENT).strong())
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(72.0, 32.0));
                if ui.add(send).clicked() {
                    submit = true;
                }
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.draft)
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .vertical_align(egui::Align::Center)
                        .margin(egui::Margin::symmetric(10, 8))
                        .hint_text("Message the channel…  (/help for commands)"),
                );
                self.compose_focus = resp.has_focus();
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter {
                    submit = true;
                    resp.request_focus();
                }
            });
        });
        if submit {
            self.send_text();
        }
    }

    fn chat_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("LAN").heading().color(theme::TEXT).strong());
            ui.label(egui::RichText::new("CHAT").heading().color(theme::ACCENT).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("[{}]", self.net.group().ip()))
                        .font(theme::label_font())
                        .color(theme::MUTED),
                );
            });
        });
        ui.add_space(10.0);

        let stick = self.scroll_to_bottom;
        Container::new().padding(4).show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .stick_to_bottom(stick)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    if self.log.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(24.0);
                            ui.label(egui::RichText::new("You're on the channel.").color(theme::TEXT));
                            ui.label(
                                egui::RichText::new("Say hello, or type /help for commands.")
                                    .color(theme::MUTED),
                            );
                        });
                    }
                    for line in &self.log {
                        if line.who == "*" {
                            ui.horizontal_wrapped(|ui| {
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new(&line.body).italics().color(theme::MUTED));
                            });
                            continue;
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.add_space(4.0);
                            let color = if line.mine { theme::ACCENT } else { theme::PEER_NAME };
                            ui.label(egui::RichText::new(&line.who).color(color).strong());
                            ui.label(egui::RichText::new(&line.body).color(theme::TEXT));
                        });
                        ui.add_space(2.0);
                    }
                    if stick {
                        ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                    }
                });
            self.scroll_to_bottom = false;
        });
    }
}
