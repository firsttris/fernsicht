// The conversion shader, compiled to SPIR-V when the crate is built.
fn main() {
    fernsicht_shader::build("src/convert.wgsl", "convert.spv");
}
