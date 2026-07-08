//! egui front-end tying networking and voice together.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::audio::Voice;
use crate::net::Net;
use crate::protocol::{Envelope, Payload};

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
        let env = Envelope::new(
            self.peer_id,
            Payload::Text { name: self.name.clone(), body: body.clone() },
        );
        let _ = self.net.send(&env);
        self.log.push(ChatLine { who: self.name.clone(), body, mine: true });
        self.draft.clear();
        self.scroll_to_bottom = true;
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
                    self.touch_peer(env.peer_id, &name, now);
                    self.peers.remove(&env.peer_id);
                    self.log.push(ChatLine {
                        who: "*".into(),
                        body: format!("{name} left"),
                        mine: false,
                    });
                    self.scroll_to_bottom = true;
                }
                Payload::Text { name, body } => {
                    self.touch_peer(env.peer_id, &name, now);
                    self.log.push(ChatLine { who: name, body, mine: false });
                    self.scroll_to_bottom = true;
                }
                Payload::Voice { name, seq: _, pcm } => {
                    self.touch_peer(env.peer_id, &name, now);
                    if let Some(p) = self.peers.get_mut(&env.peer_id) {
                        p.speaking_until = now + Duration::from_millis(300);
                    }
                    if let Some(v) = &self.voice {
                        v.playback.push_frame(&pcm, v.out_rate);
                    }
                }
            }
        }
    }

    fn touch_peer(&mut self, id: u64, name: &str, now: Instant) {
        let is_new = !self.peers.contains_key(&id);
        let entry = self.peers.entry(id).or_insert_with(|| Peer {
            name: name.to_string(),
            last_seen: now,
            speaking_until: now,
        });
        entry.name = name.to_string();
        entry.last_seen = now;
        if is_new {
            self.log.push(ChatLine {
                who: "*".into(),
                body: format!("{name} joined"),
                mine: false,
            });
            self.scroll_to_bottom = true;
        }
    }

    fn pump_voice(&mut self) {
        // Take frames from the mic and broadcast them.
        let mut frames = Vec::new();
        if let Some(v) = &self.voice {
            while let Ok(frame) = v.frames_rx.try_recv() {
                frames.push(frame);
            }
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_network();
        self.pump_voice();
        self.housekeeping();

        egui::SidePanel::right("peers").min_width(180.0).show(ctx, |ui| {
            ui.heading("Peers");
            ui.separator();
            ui.label(egui::RichText::new(format!("● {} (you)", self.name)).strong());
            let now = Instant::now();
            let mut peers: Vec<&Peer> = self.peers.values().collect();
            peers.sort_by(|a, b| a.name.cmp(&b.name));
            for p in peers {
                let speaking = p.speaking_until > now;
                let dot = if speaking { "🔊" } else { "●" };
                ui.label(format!("{dot} {}", p.name));
            }
            ui.separator();
            ui.label(
                egui::RichText::new(format!("group\n[{}]", self.net.group().ip()))
                    .small()
                    .weak(),
            );

            ui.add_space(8.0);
            match &mut self.voice {
                Some(v) => {
                    let mut on = v.mic_on();
                    if ui.checkbox(&mut on, "🎤 Transmit voice").changed() {
                        if let Err(e) = v.set_mic(on) {
                            self.voice_err = Some(e);
                        }
                    }
                }
                None => {
                    ui.colored_label(egui::Color32::YELLOW, "voice unavailable");
                }
            }
            if let Some(e) = &self.voice_err {
                ui.colored_label(egui::Color32::LIGHT_RED, e);
            }
        });

        egui::TopBottomPanel::bottom("compose").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.draft)
                        .desired_width(f32::INFINITY)
                        .hint_text("Message the LAN…"),
                );
                let enter =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter {
                    self.send_text();
                    resp.request_focus();
                }
                if ui.button("Send").clicked() {
                    self.send_text();
                }
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("LAN Chat  ·  IPv6 multicast");
            ui.separator();
            let mut scroll = egui::ScrollArea::vertical().auto_shrink([false; 2]);
            if self.scroll_to_bottom {
                scroll = scroll.stick_to_bottom(true);
            }
            scroll.show(ui, |ui| {
                for line in &self.log {
                    if line.who == "*" {
                        ui.label(egui::RichText::new(&line.body).italics().weak());
                        continue;
                    }
                    ui.horizontal_wrapped(|ui| {
                        let name = egui::RichText::new(format!("{}:", line.who)).strong().color(
                            if line.mine {
                                egui::Color32::LIGHT_GREEN
                            } else {
                                egui::Color32::LIGHT_BLUE
                            },
                        );
                        ui.label(name);
                        ui.label(&line.body);
                    });
                }
            });
            self.scroll_to_bottom = false;
        });

        // Keep polling network/voice even when idle.
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self
            .net
            .send(&Envelope::new(self.peer_id, Payload::Bye { name: self.name.clone() }));
    }
}
