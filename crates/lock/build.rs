//! Decides whether libpam is available. `cfg(aurora_pam)` selects the real authenticator;
//! without it the stub is built, which refuses every unlock (fail closed).
use std::path::Path;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(aurora_pam)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=AURORA_LIBPAM_DIR");
    if std::env::var_os("CARGO_FEATURE_PAM").is_none() {
        println!("cargo::warning=aurora-lock built WITHOUT PAM: it can never unlock (fail closed)");
        return;
    }
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(d) = std::env::var("AURORA_LIBPAM_DIR") {
        dirs.push(d);
    }
    for d in [
        "/usr/lib",
        "/usr/lib64",
        "/lib",
        "/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
    ] {
        dirs.push(d.into());
    }
    for d in dirs {
        if Path::new(&d).join("libpam.so").exists() {
            println!("cargo::rustc-link-search=native={d}");
            println!("cargo::rustc-link-lib=pam");
            println!("cargo::rustc-cfg=aurora_pam");
            return;
        }
    }
    println!(
        "cargo::warning=libpam.so not found: aurora-lock is built WITHOUT PAM and can never unlock (fail closed). Set AURORA_LIBPAM_DIR."
    );
}
