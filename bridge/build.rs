fn main() {
    println!("cargo:rerun-if-changed=src/audio.c");
    println!("cargo:rerun-if-changed=src/video.c");
    let mut build = cc::Build::new();
    build.file("src/audio.c");
    build.file("src/video.c").flag_if_supported("-std=c11");
    for name in ["libavcodec", "libavutil", "libswscale"] {
        let library = pkg_config::Config::new()
            .probe(name)
            .unwrap_or_else(|e| panic!("FFmpeg development library {name} required: {e}"));
        for path in library.include_paths {
            build.include(path);
        }
    }
    build.compile("niri_rdp_video");
}
