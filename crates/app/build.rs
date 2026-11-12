//! Compiles the interface, its translations bundled in the binary.

fn main() -> Result<(), slint_build::CompileError> {
    // The element tree the tests search (`ElementHandle`): debug builds only.
    let debug = std::env::var("PROFILE").is_ok_and(|profile| profile == "debug");
    let configuration = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("lang")
        .with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", configuration)
}
