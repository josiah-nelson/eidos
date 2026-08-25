fn main() {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
    {
        if output.status.success() {
            let revision = String::from_utf8_lossy(&output.stdout);
            println!("cargo:rustc-env=EIDOS_BUILD_REVISION={}", revision.trim());
        }
    }
    println!("cargo:rerun-if-changed=src/endpoint_security_shim.c");
    println!("cargo:rerun-if-changed=src/file_metadata.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/file_metadata.m")
            .compile("eidos_file_metadata");
        println!("cargo:rustc-link-lib=framework=Foundation");
    }
    if std::env::var_os("CARGO_FEATURE_ENDPOINT_SECURITY").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
    {
        cc::Build::new()
            .file("src/endpoint_security_shim.c")
            .flag("-fblocks")
            .compile("eidos_endpoint_security");
        println!("cargo:rustc-link-lib=EndpointSecurity");
    }
}
