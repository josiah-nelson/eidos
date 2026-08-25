//! Tracks the built web UI so an embedded copy is refreshed when `web/dist`
//! changes, and lets release builds insist on its presence.
//!
//! `EIDOS_REQUIRE_WEB=1` fails the build when `web/dist/index.html` is
//! missing, so a packaged executable can never silently ship without its UI.

use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-env-changed=EIDOS_REQUIRE_WEB");
    let present = dist.join("index.html").is_file();
    let required = std::env::var_os("EIDOS_REQUIRE_WEB").is_some_and(|v| v == "1");
    match (present, required) {
        (false, true) => panic!(
            "EIDOS_REQUIRE_WEB=1 but {} has no index.html; build the web UI first (cd web; npm ci; npm run build)",
            dist.display()
        ),
        (false, false) => println!(
            "cargo:warning=web/dist not found; the executable will embed no web UI (API only unless --web-dir is given)"
        ),
        _ => {}
    }
}
