//! LAN Chat — IPv6 multicast chat & voice.
//!
//! Shared library crate used by the desktop binary (`main.rs`) and the Android
//! entry point (`android_main`, below).

pub mod app;
pub mod audio;
pub mod net;
pub mod protocol;
pub mod theme;

use eframe::egui;

use crate::app::ChatApp;
use crate::net::Net;

/// Decode the bundled window icon (used on desktop; harmless elsewhere).
fn window_icon() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).ok()
}

/// A viewport builder with title + icon.
pub fn base_viewport() -> egui::ViewportBuilder {
    let mut vp = egui::ViewportBuilder::default()
        .with_inner_size([760.0, 520.0])
        .with_title("LAN Chat · IPv6");
    if let Some(icon) = window_icon() {
        vp = vp.with_icon(icon);
    }
    vp
}

/// Native options for the desktop app.
pub fn desktop_options() -> eframe::NativeOptions {
    eframe::NativeOptions { viewport: base_viewport(), ..Default::default() }
}

/// Run the egui app to completion.
pub fn launch(
    name: String,
    net: Net,
    peer_id: u64,
    options: eframe::NativeOptions,
) -> eframe::Result<()> {
    eframe::run_native(
        "lan_chat",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(ChatApp::new(peer_id, name, net)))
        }),
    )
}

// ---- Android entry point ----------------------------------------------------

/// Acquire a Wi-Fi `MulticastLock` — mandatory on Android, which otherwise
/// silently drops inbound multicast. Held for the process lifetime.
#[cfg(target_os = "android")]
mod android_multicast {
    use jni::objects::{GlobalRef, JObject};
    use jni::JavaVM;
    use std::sync::OnceLock;

    static LOCK: OnceLock<GlobalRef> = OnceLock::new();

    pub fn acquire() {
        match try_acquire() {
            Ok(()) => log::info!("acquired Wi-Fi MulticastLock"),
            Err(e) => log::warn!("could not acquire MulticastLock (RX may fail): {e}"),
        }
    }

    fn try_acquire() -> Result<(), Box<dyn std::error::Error>> {
        let ctx = ndk_context::android_context();
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }?;
        let mut env = vm.attach_current_thread()?;
        let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

        // WifiManager wifi = (WifiManager) ctx.getSystemService("wifi");
        let svc = env.new_string("wifi")?;
        let wifi = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&svc).into()],
            )?
            .l()?;

        // MulticastLock lock = wifi.createMulticastLock("lan_chat");
        let tag = env.new_string("lan_chat")?;
        let lock = env
            .call_method(
                &wifi,
                "createMulticastLock",
                "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;",
                &[(&tag).into()],
            )?
            .l()?;

        env.call_method(&lock, "setReferenceCounted", "(Z)V", &[jni::objects::JValue::Bool(0)])?;
        env.call_method(&lock, "acquire", "()V", &[])?;

        let _ = LOCK.set(env.new_global_ref(lock)?);
        Ok(())
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: android_activity::AndroidApp) {
    use android_logger::Config;

    android_logger::init_once(
        Config::default().with_max_level(log::LevelFilter::Info).with_tag("lan_chat"),
    );

    android_multicast::acquire();

    let name = gethostname::gethostname().to_string_lossy().to_string();
    let name = if name.is_empty() || name == "localhost" { "android".to_string() } else { name };
    let peer_id: u64 = rand::random();

    let net = match Net::join(net::DEFAULT_GROUP, net::DEFAULT_PORT, 0) {
        Ok(n) => n,
        Err(e) => {
            log::error!("failed to join multicast group: {e}");
            return;
        }
    };
    log::info!("android: joined as {name} (peer {peer_id:016x})");

    let options = eframe::NativeOptions {
        android_app: Some(app),
        viewport: base_viewport(),
        ..Default::default()
    };
    if let Err(e) = launch(name, net, peer_id, options) {
        log::error!("eframe exited: {e}");
    }
}
