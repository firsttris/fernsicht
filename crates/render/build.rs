// The renderer's shader, compiled to SPIR-V when the crate is built with
// the Vulkan renderer.
fn main() {
    #[cfg(feature = "vulkan")]
    fernsicht_shader::build("src/vulkan/shader.wgsl", "shader.spv");
}
