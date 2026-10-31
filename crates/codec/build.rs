//! Which FFmpeg the codecs are built against, where the API differs.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_queue_families)");
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_9)");
    // FFmpeg 7.1 lists a Vulkan device's queue families (`qf`, `nb_qf`);
    // before, only the fixed per-purpose fields exist. Set by
    // ffmpeg-sys-next (a dependency with the vulkan feature).
    if std::env::var("DEP_FFMPEG_FFMPEG_7_1").as_deref() == Ok("true") {
        println!("cargo::rustc-cfg=ffmpeg_queue_families");
    }
    // FFmpeg 9 drops the fixed per-purpose queue fields (only `qf` is left)
    // and keeps a frame's access mask as VkAccessFlagBits2 (64 bits).
    if std::env::var("DEP_FFMPEG_FFMPEG_9_0").as_deref() == Ok("true") {
        println!("cargo::rustc-cfg=ffmpeg_9");
    }
}
