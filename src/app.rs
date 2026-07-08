//! egui front-end tying networking and voice together.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::audio::{Cue, Voice};
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

pub struct ChatApp {
    peer_id: u64,
    name: String,
    net: Net,
    voice: Option<Voice>,
    voice_err: Option<String>,
    voice_seq: u32,

    draft: String,
    log: Vec<ChatLine>,
    peers: HashMap<u64, Peer>,
    last_hello: Instant,
    scroll_to_bottom: bool,
    quit: bool,
    compose_focus: bool,

    /// Smoothed outbound mic level (0..1) for the transmit VU.
    mic_level: f32,
    /// Smoothed inbound level (0..1) per speaking peer, for their VU.
    peer_levels: HashMap<u64, f32>,

    input_devices: Vec<String>,
    output_devices: Vec<String>,
}

/// Peak of a mono frame, lightly boosted so normal speech is clearly visible.
fn peak_level(samples: &[f32]) -> f32 {
    let p = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    (p * 3.0).min(1.0)
}

impl ChatApp {
    pub fn new(peer_id: u64, name: String, net: Net) -> Self {
        // Announce ourselves immediately.
        let _ = net.send(&Envelope::new(peer_id, Payload::Hello { name: name.clone() }));

        let (voice, voice_err) = match Voice::new() {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };

        ChatApp {
            quit: false,
            compose_focus: false,
            mic_level: 0.0,
            peer_levels: HashMap::new(),
            input_devices: crate::audio::list_input_devices(),
            output_devices: crate::audio::list_output_devices(),
            peer_id,
            name,
            net,
            voice,
            voice_err,
            voice_seq: 0,
            draft: String::new(),
            log: Vec::new(),
            peers: HashMap::new(),
            last_hello: Instant::now() - Duration::from_secs(10),
            scroll_to_bottom: false,
        }
    }

    fn send_text(&mut self) {
        let body = self.draft.trim().to_string();
        if body.is_empty() {
            return;
        }
        self.draft.clear();

        // A leading '/' (but not '//') is a chat command.
        if let Some(rest) = body.strip_prefix('/') {
            if !rest.starts_with('/') {
                self.handle_command(rest);
                return;
            }
            // "//text" escapes to a literal message starting with '/'.
            let body = rest.to_string();
            self.broadcast_text(body);
            return;
        }
        self.broadcast_text(body);
    }

    fn broadcast_text(&mut self, body: String) {
        let env = Envelope::new(
            self.peer_id,
            Payload::Text { name: self.name.clone(), body: body.clone() },
        );
        let _ = self.net.send(&env);
        self.log.push(ChatLine { who: self.name.clone(), body, mine: true });
        self.scroll_to_bottom = true;
        self.play_cue(Cue::Message);
    }

    /// Push a local-only system line (not sent over the network).
    fn sys(&mut self, msg: impl Into<String>) {
        self.log.push(ChatLine { who: "*".into(), body: msg.into(), mine: false });
        self.scroll_to_bottom = true;
    }

    /// Play a notification cue (no-op if no audio output is available).
    fn play_cue(&self, cue: Cue) {
        if let Some(v) = &self.voice {
            v.playback.play_cue(cue);
        }
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
                    let old = std::mem::replace(&mut self.name, arg.clone());
                    let _ = self.net.send(&Envelope::new(
                        self.peer_id,
                        Payload::Hello { name: self.name.clone() },
                    ));
                    self.sys(format!("{old} is now known as {arg}"));
                }
            }
            "me" => {
                if arg.is_empty() {
                    self.sys("usage: /me <action>");
                } else {
                    let _ = self.net.send(&Envelope::new(
                        self.peer_id,
                        Payload::Emote { name: self.name.clone(), action: arg.clone() },
                    ));
                    self.sys(format!("{} {}", self.name, arg));
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
                let mut names: Vec<String> = self.peers.values().map(|p| p.name.clone()).collect();
                names.sort();
                if names.is_empty() {
                    self.sys("no other peers on the group");
                } else {
                    self.sys(format!("{} peer(s): {}", names.len(), names.join(", ")));
                }
            }
            "clear" => {
                self.log.clear();
            }
            "quit" | "exit" => {
                self.quit = true;
            }
            "help" | "?" => {
                self.sys("commands: /name <n>, /me <action>, /mic [on|off], /peers, /clear, /quit, /help");
            }
            other => {
                self.sys(format!("unknown command: /{other}  (try /help)"));
            }
        }
    }

    fn pump_network(&mut self) {
        while let Ok((env, _from)) = self.net.incoming.try_recv() {
            if env.peer_id == self.peer_id {
                continue; // ignore our own looped-back traffic
            }
            let now = Instant::now();
            match env.payload {
                Payload::Hello { name } => {
                    self.touch_peer(env.peer_id, &name, now);
                }
                Payload::Bye { name } => {
                    // Only announce a departure for a peer we actually knew.
                    if self.peers.remove(&env.peer_id).is_some() {
                        self.log.push(ChatLine {
                            who: "*".into(),
                            body: format!("{name} left"),
                            mine: false,
                        });
                        self.scroll_to_bottom = true;
                        self.play_cue(Cue::Leave);
                    }
                }
                Payload::Text { name, body } => {
                    self.touch_peer(env.peer_id, &name, now);
                    self.log.push(ChatLine { who: name, body, mine: false });
                    self.scroll_to_bottom = true;
                    self.play_cue(Cue::Message);
                }
                Payload::Emote { name, action } => {
                    self.touch_peer(env.peer_id, &name, now);
                    self.log.push(ChatLine {
                        who: "*".into(),
                        body: format!("{name} {action}"),
                        mine: false,
                    });
                    self.scroll_to_bottom = true;
                }
                Payload::Voice { name, seq: _, pcm } => {
                    self.touch_peer(env.peer_id, &name, now);
                    if let Some(p) = self.peers.get_mut(&env.peer_id) {
                        p.speaking_until = now + Duration::from_millis(300);
                    }
                    // Track inbound level (attack) so the peer's VU confirms
                    // we are actually receiving their audio.
                    let lvl = peak_level(&pcm);
                    let e = self.peer_levels.entry(env.peer_id).or_insert(0.0);
                    *e = e.max(lvl);
                    if let Some(v) = &self.voice {
                        v.playback.push_frame(env.peer_id, &pcm);
                    }
                }
            }
        }
    }

    fn touch_peer(&mut self, id: u64, name: &str, now: Instant) {
        let is_new = !self.peers.contains_key(&id);
        {
            let entry = self.peers.entry(id).or_insert_with(|| Peer {
                name: name.to_string(),
                last_seen: now,
                speaking_until: now,
            });
            entry.name = name.to_string();
            entry.last_seen = now;
        }
        if is_new {
            self.log.push(ChatLine {
                who: "*".into(),
                body: format!("{name} joined"),
                mine: false,
            });
            self.scroll_to_bottom = true;
            self.play_cue(Cue::Join);
        }
    }

    /// Release levels toward zero every frame; attacks happen on packet/frame
    /// arrival. Drops peers that have gone quiet.
    fn decay_levels(&mut self) {
        self.mic_level *= 0.80;
        if self.mic_level < 0.001 {
            self.mic_level = 0.0;
        }
        self.peer_levels.retain(|_, l| {
            *l *= 0.82;
            *l > 0.01
        });
    }

    fn pump_voice(&mut self) {
        // Take frames from the mic and broadcast them.
        let mut frames = Vec::new();
        if let Some(v) = &self.voice {
            while let Ok(frame) = v.frames_rx.try_recv() {
                frames.push(frame);
            }
        }
        // Outbound mic level (attack) for the transmit VU.
        for frame in &frames {
            self.mic_level = self.mic_level.max(peak_level(frame));
        }
        for frame in frames {
            let seq = self.voice_seq;
            self.voice_seq = self.voice_seq.wrapping_add(1);
            let env = Envelope::new(
                self.peer_id,
                Payload::Voice { name: self.name.clone(), seq, pcm: frame },
            );
            let _ = self.net.send(&env);
        }
    }

    fn housekeeping(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_hello) > Duration::from_secs(5) {
            let _ = self
                .net
                .send(&Envelope::new(self.peer_id, Payload::Hello { name: self.name.clone() }));
            self.last_hello = now;
        }
        // Expire peers we haven't heard from in a while.
        self.peers
            .retain(|_, p| now.duration_since(p.last_seen) < Duration::from_secs(15));
    }
}

impl eframe::App for ChatApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.decay_levels();
        self.pump_network();
        self.pump_voice();
        self.housekeeping();
        if self.quit {
            let _ = self.net.send(&Envelope::new(
                self.peer_id,
                Payload::Bye { name: self.name.clone() },
            ));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Keep polling network/voice even when idle.
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
            .send(&Envelope::new(self.peer_id, Payload::Bye { name: self.name.clone() }));
    }
}

impl ChatApp {
    fn peers_card(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let mic_on = self.voice.as_ref().map_or(false, |v| v.mic_on());
        let mic_level = self.mic_level;
        Container::titled("Peers").show(ui, |ui| {
            // Yourself, always first and in the accent colour.
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("●").color(theme::ACCENT));
                ui.label(egui::RichText::new(&self.name).color(theme::ACCENT).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(if mic_on { "tx" } else { "you" })
                            .font(theme::label_font())
                            .color(if mic_on { theme::ACCENT } else { theme::MUTED }),
                    );
                });
            });
            // Your outbound level (only meaningful while transmitting).
            theme::vu_meter(ui, if mic_on { mic_level } else { 0.0 });
            ui.add_space(8.0);

            let mut peers: Vec<(&u64, &Peer)> = self.peers.iter().collect();
            peers.sort_by(|a, b| a.1.name.cmp(&b.1.name));
            for (id, p) in &peers {
                let speaking = p.speaking_until > now;
                ui.horizontal(|ui| {
                    let dot = if speaking { theme::ACCENT } else { theme::BORDER };
                    ui.label(egui::RichText::new("●").color(dot));
                    ui.label(egui::RichText::new(&p.name).color(theme::PEER_NAME));
                    if speaking {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new("live")
                                    .font(theme::label_font())
                                    .color(theme::ACCENT),
                            );
                        });
                    }
                });
                // Inbound level: confirms we're actually receiving their audio.
                let level = self.peer_levels.get(*id).copied().unwrap_or(0.0);
                theme::vu_meter(ui, level);
                ui.add_space(8.0);
            }
            if peers.is_empty() {
                ui.label(
                    egui::RichText::new("waiting for peers…")
                        .italics()
                        .color(theme::MUTED),
                );
            }
        });
    }

    fn channel_card(&mut self, ui: &mut egui::Ui) {
        let group = self.net.group();
        Container::titled("Channel").padding(12).show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("[{}]", group.ip()))
                    .monospace()
                    .color(theme::TEXT),
            );
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
        let mic_level = self.mic_level;

        Container::titled("Voice").show(ui, |ui| {
            match &self.voice {
                Some(v) => {
                    let on = v.mic_on();
                    // Stateful primary control: teal while transmitting.
                    let label = if on { "◉  Transmitting" } else { "◎  Transmit voice" };
                    let btn = egui::Button::new(
                        egui::RichText::new(label)
                            .color(if on { theme::ON_ACCENT } else { theme::TEXT }),
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
                                if ui
                                    .selectable_label(v.input_name().as_deref() == Some(d), d)
                                    .clicked()
                                {
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
                                if ui
                                    .selectable_label(v.output_name().as_deref() == Some(d), d)
                                    .clicked()
                                {
                                    new_out = Some(Some(d.clone()));
                                }
                            }
                        });

                    ui.add_space(8.0);
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("⟳  Rescan devices").color(theme::MUTED),
                        ))
                        .clicked()
                    {
                        refresh = true;
                    }
                }
                None => {
                    ui.label(
                        egui::RichText::new("No audio device — chat still works.")
                            .color(theme::MUTED),
                    );
                }
            }
            if let Some(e) = &voice_err {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::from_rgb(0xff, 0x6b, 0x6b), e);
            }
        });

        // Apply deferred mutations (kept out of the &self borrow above).
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
            // Right-to-left: the Send button anchors right, the input fills the
            // remaining width to its left.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let send = egui::Button::new(
                    egui::RichText::new("Send").color(theme::ON_ACCENT).strong(),
                )
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
        // Header: wordmark + live channel address.
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

        Container::new().padding(4).show(ui, |ui| {
            let mut scroll = egui::ScrollArea::vertical().auto_shrink([false; 2]);
            if self.scroll_to_bottom {
                scroll = scroll.stick_to_bottom(true);
            }
            scroll.show(ui, |ui| {
                ui.add_space(4.0);
                if self.log.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0);
                        ui.label(
                            egui::RichText::new("You're on the channel.").color(theme::TEXT),
                        );
                        ui.label(
                            egui::RichText::new("Say hello, or type /help for commands.")
                                .color(theme::MUTED),
                        );
                    });
                }
                for line in &self.log {
                    if line.who == "*" {
                        // System / emote line.
                        ui.horizontal_wrapped(|ui| {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(&line.body).italics().color(theme::MUTED),
                            );
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
            });
            self.scroll_to_bottom = false;
        });
    }
}
