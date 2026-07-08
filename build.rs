//! Embed the application icon into the Windows executable so Explorer, the
//! taskbar, and Alt-Tab show it. No-op on non-Windows targets.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            eprintln!("winresource: failed to embed icon: {e}");
        }
    }
}
