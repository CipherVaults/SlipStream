use std::{env, fs, process};

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--version"] {
        println!("2025.01.01");
        return;
    }
    assert!(args.iter().any(|arg| arg == "--yes-playlist"));
    if args.iter().any(|arg| arg == "--dump-single-json") {
        println!(
            r#"{{"_type":"playlist","title":"Recovery","entries":[{{"title":"MP4"}},{{"title":"Transport Stream"}},{{"title":"Unreadable"}},{{"title":"Unmerged"}}]}}"#
        );
        return;
    }
    assert!(args.iter().any(|arg| arg == "--ignore-errors"));
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
