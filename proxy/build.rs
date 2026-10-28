//! GNU ld (Linux) reads libraries once, in order. libduckdb-sys links its extension
//! registration archive after libduckdb, so the function that archive calls is no longer
//! available by then; asking for it up front keeps it in the binary. macOS's linker and lld
//! do not need this.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,--undefined=duckdb_register_static_extensions");
    }
}
