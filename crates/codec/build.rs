//! Which FFmpeg the codecs are built against, where the API differs.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_queue_families)");
    // FFmpeg 7.1 lists a Vulkan device's queue families (`qf`, `nb_qf`);
    // before, only the fixed per-purpose fields exist. Set by
    // ffmpeg-sys-next (a dependency with the vulkan feature).
    if std::env::var("DEP_FFMPEG_FFMPEG_7_1").as_deref() == Ok("true") {
        println!("cargo::rustc-cfg=ffmpeg_queue_families");
    }
}
