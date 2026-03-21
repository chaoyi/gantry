use std::path::Path;

fn main() {
    let elk_path = Path::new("ui/elk.bundled.js");
    if !elk_path.exists() {
        eprintln!("Downloading ELK.js...");
        let output = std::process::Command::new("wget")
            .args([
                "-qO",
                "ui/elk.bundled.js",
                "https://cdn.jsdelivr.net/npm/elkjs@0.9.3/lib/elk.bundled.js",
            ])
            .output()
            .expect("failed to download ELK.js — install wget or place ui/elk.bundled.js manually");
        if !output.status.success() {
            panic!(
                "failed to download ELK.js: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    println!("cargo:rerun-if-changed=ui/elk.bundled.js");
}
