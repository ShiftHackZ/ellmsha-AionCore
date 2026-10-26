fn main() {
    // Track additions and removals, not only files already known to RustEmbed.
    println!("cargo:rerun-if-changed=assets/logos");
}
