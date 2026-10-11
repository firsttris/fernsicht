//! WGSL to SPIR-V for build scripts: the shaders are compiled when the
//! crate that runs them is built (a mistake in one fails the build), and
//! the programs carry the SPIR-V instead of a shader compiler.
//!
//! ```ignore
//! // build.rs
//! fernsicht_shader::build("src/convert.wgsl", "convert.spv");
//! // the crate
//! const SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/convert.spv"));
//! ```

use std::path::Path;

use naga::back::spv;
use naga::valid::{Capabilities, ValidationFlags, Validator};

/// Compiles WGSL to SPIR-V words, for Vulkan's clip space (no y flip).
pub fn compile(wgsl: &str) -> Result<Vec<u32>, String> {
    let module = naga::front::wgsl::parse_str(wgsl).map_err(|e| e.emit_to_string(wgsl))?;
    let info = Validator::new(ValidationFlags::all(), Capabilities::IMMEDIATES)
        .validate(&module)
        .map_err(|e| e.emit_to_string(wgsl))?;
    let mut options = spv::Options::default();
    options
        .flags
        .remove(spv::WriterFlags::ADJUST_COORDINATE_SPACE);
    spv::write_vec(&module, &info, &options, None).map_err(|e| e.to_string())
}

/// For a build script: compiles `wgsl` (a path in the crate) into
/// `$OUT_DIR/spv` (little-endian words), and rebuilds when it changes.
/// Panics with the compiler's message, which fails the build.
pub fn build(wgsl: &str, spv: &str) {
    println!("cargo:rerun-if-changed={wgsl}");
    let out = std::env::var_os("OUT_DIR").expect("run from a build script");
    if let Err(e) = write_spirv(Path::new(wgsl), &Path::new(&out).join(spv)) {
        panic!("{e}");
    }
}

/// Compiles the WGSL file `wgsl` into the SPIR-V file `spv`.
pub fn write_spirv(wgsl: &Path, spv: &Path) -> Result<(), String> {
    let name = wgsl.display();
    let source = std::fs::read_to_string(wgsl).map_err(|e| format!("reading {name}: {e}"))?;
    let words = compile(&source).map_err(|e| format!("{name}:\n{e}"))?;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    std::fs::write(spv, bytes).map_err(|e| format!("writing {}: {e}", spv.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shader_compiles_and_a_mistake_says_where() {
        let ok = "@compute @workgroup_size(1) fn main() {}";
        assert_eq!(compile(ok).unwrap()[0], 0x0723_0203, "SPIR-V magic");
        let e = compile("fn main( {}").unwrap_err();
        assert!(e.contains("expected"), "{e}");
    }

    #[test]
    fn a_file_compiles_into_little_endian_words() {
        let dir = std::env::temp_dir().join(format!("fernsicht-shader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (wgsl, spv) = (dir.join("a.wgsl"), dir.join("a.spv"));
        std::fs::write(&wgsl, "@compute @workgroup_size(1) fn main() {}").unwrap();
        write_spirv(&wgsl, &spv).unwrap();
        let bytes = std::fs::read(&spv).unwrap();
        assert_eq!(bytes[..4], [0x03, 0x02, 0x23, 0x07], "magic, little-endian");
        std::fs::write(&wgsl, "nonsense").unwrap();
        assert!(write_spirv(&wgsl, &spv).unwrap_err().contains("a.wgsl"));
        assert!(write_spirv(&dir.join("none.wgsl"), &spv).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
