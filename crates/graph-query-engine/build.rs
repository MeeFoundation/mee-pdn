//! Links `LadybugDB`'s vector extension into every binary built against this crate, where the
//! engine was built with it (`scripts/lbug-vector-source.sh`): a library this crate names reaches
//! every final artifact that depends on it, Android's `cdylib` included, which a `rustc-link-arg`
//! would not.

use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(lbug_vector)");
    println!("cargo:rerun-if-env-changed=LBUG_SOURCE_DIR");
    let Some(root) = std::env::var_os("LBUG_SOURCE_DIR").map(PathBuf::from) else {
        return;
    };
    let config = root.join("extension/extension_config.cmake");
    println!("cargo:rerun-if-changed={}", config.display());
    let linked = std::fs::read_to_string(&config)
        .is_ok_and(|c| c.contains("add_static_link_extension(vector)"));
    if !linked {
        return;
    }
    println!("cargo:rustc-link-lib=static:+whole-archive,-bundle=vector_static");
    println!("cargo:rustc-cfg=lbug_vector");
}
