//! Compiles the interface, its translations bundled in the binary.

fn main() -> Result<(), slint_build::CompileError> {
    let configuration = slint_build::CompilerConfiguration::new().with_bundled_translations("lang");
    slint_build::compile_with_config("ui/app.slint", configuration)
}
