fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icon/hamster-wheel.rc");
    println!("cargo:rerun-if-changed=icon/hamster-wheel.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Only the shipped executable gets the icon, not libraries/benches.
        // Fail instead of silently producing a Windows binary without it.
        embed_resource::compile_for(
            "icon/hamster-wheel.rc",
            ["hamster-wheel"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("compile the hamster-wheel Windows icon resource (Windows SDK required)");
    }
}
