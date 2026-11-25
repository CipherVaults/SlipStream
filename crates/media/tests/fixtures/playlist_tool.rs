use std::{env, fs, process};

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--version"] || args == ["-version"] {
        println!("2025.01.01");
        return;
    }
    if args.iter().any(|arg| arg == "--no-playlist") {
        single_video(&args);
        return;
    }
    assert!(args.iter().any(|arg| arg == "--yes-playlist"));
    if args.iter().any(|arg| arg == "--dump-single-json") {
        fs::write(env::current_exe().unwrap().parent().unwrap().join("playlist-probed"), "yes").unwrap();
        println!(
            r#"{{"_type":"playlist","title":"Recovery","entries":[{{"title":"MP4"}},{{"title":"Transport Stream"}},{{"title":"Unreadable"}},{{"title":"Unmerged"}}]}}"#
        );
        return;
    }
    assert!(args.iter().any(|arg| arg == "--ignore-errors"));
    if let Some(items) = args.windows(2).find(|pair| pair[0] == "--playlist-items") {
        assert_eq!(items[1], "2");
        fs::write("media-000002-000001.webm", "single playlist item").unwrap();
        return;
    }
    let executable = env::current_exe().unwrap();
    let fixtures = executable.parent().unwrap();
    for (source, staged) in [
        ("synthetic.mp4", "media-000001-000001.mp4"),
        ("hls-segments.mp4", "media-000002-000002.mp4"),
        ("disguised.mp4", "media-000003-000003.mp4"),
        ("synthetic.mp4", "media-000004-000004.f137.mp4"),
        ("synthetic.mp4", "media-000004-000004.temp.mp4"),
    ] {
        fs::copy(fixtures.join(source), staged).unwrap();
    }
    eprintln!("ERROR: playlist postprocessing failed");
    let status = args.last().unwrap().rsplit('/').next().unwrap();
    process::exit(status.parse().unwrap());
}

fn single_video(args: &[String]) {
    let option = |flag| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
    };
    let executable = env::current_exe().unwrap();
    let dir = executable.parent().unwrap();
    let cache = dir.join("probe-cache");
    assert_eq!(
        option("--cache-dir").map(std::path::Path::new),
        Some(cache.as_path())
    );
    assert_eq!(option("--add-header"), Some("User-Agent:SlipStream Test"));
    assert!(!args.iter().any(|arg| arg == "--no-cache-dir"));

    if args.iter().any(|arg| arg == "--dump-single-json") {
        assert!(option("--ffmpeg-location").is_none());
        fs::write(dir.join("probed"), "yes").unwrap();
        println!(r#"{{"title":"Fresh video"}}"#);
    } else {
        assert_eq!(
            option("-f"),
            Some("bestvideo*[height<=720]+bestaudio/bestvideo*[height<=720]")
        );
        assert!(option("--ffmpeg-location").is_some());
        fs::write("media-000001.mp4", "single-video fixture").unwrap();
    }
}
