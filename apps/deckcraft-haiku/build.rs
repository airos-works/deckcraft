//! Builds the native host (`native/host.cpp`, and the printing in `native/print.cpp`) against the Be API when the target is Haiku (air/OS
//! is a Haiku distribution); elsewhere the crate is only the platform-neutral rasteriser and key
//! tables, and its binary says it needs Haiku.

fn main() {
    println!("cargo:rerun-if-changed=native/host.cpp");
    println!("cargo:rerun-if-changed=native/media.cpp");
    println!("cargo:rerun-if-changed=native/print.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("haiku") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .file("native/host.cpp")
        // The Media Kit's codecs for movies DeckCraft doesn't decode itself (src/media_kit.rs).
        .file("native/media.cpp")
        // The print kit (src/print.rs).
        .file("native/print.cpp")
        .flag("-Wno-multichar")
        .flag("-Wno-unused-parameter")
        .opt_level(2)
        .compile("deckcraft_haiku_host");
    // game: BDirectWindow, which lets frames go straight into the screen (src/present.rs).
    for lib in ["be", "game", "tracker", "media", "stdc++"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
