use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    thread::sleep,
    time::{Duration, Instant},
};

fn log(dir: &Path, line: &str) {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("media-tool.log"))
        .unwrap()
        .write_all(format!("{line}\n").as_bytes())
        .unwrap();
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let executable = std::env::current_exe().unwrap();
    let dir = executable.parent().unwrap();
    let url = args.last().map(String::as_str).unwrap_or_default();
    let playlist = args.iter().any(|arg| arg == "--yes-playlist");
    if args.iter().any(|arg| arg == "--yes-playlist" || arg == "--no-playlist") {
        let cache_dir = args.windows(2).find(|pair| pair[0] == "--cache-dir").unwrap();
        let cache_dir = Path::new(&cache_dir[1]);
        assert!(cache_dir.is_absolute() && cache_dir.is_dir());
        assert_eq!(cache_dir.file_name().unwrap(), "yt-dlp-cache");
        assert!(!args.iter().any(|arg| arg == "--no-cache-dir"));
    }
    if let Some(pair) = args.windows(2).find(|pair| pair[0] == "--ffmpeg-location") {
        assert!(!pair[1].starts_with(r"\\?\"));
    }
    if args.iter().any(|arg| arg == "--version") {
        log(dir, "yt-dlp version");
        println!("yt-dlp fixture 1.0");
    } else if args.iter().any(|arg| arg == "-version") {
        log(dir, "ffmpeg version");
        println!("ffmpeg fixture 1.0");
    } else if args.iter().any(|arg| arg == "--dump-single-json") {
        log(dir, &format!("probe {playlist} {url}"));
        assert!(!args.iter().any(|arg| arg == "--ffmpeg-location"));
        if url.ends_with("/coalesced") {
            sleep(Duration::from_millis(350));
        }
        if playlist && args.last().is_some_and(|url| url.ends_with("list=PL6000")) {
            let entry = format!(r#"{{"title":"{}"}}"#, "Title ".repeat(100));
            let entries = vec![entry.as_str(); 6000].join(",");
            println!(r#"{{"_type":"playlist","entries":[{entries}]}}"#);
        } else if playlist && url.ends_with("list=PL-links") {
            println!("{}", r#"{"title":"Linked: playlist","_type":"playlist","entries":[{"title":"One: video","playlist_index":2,"webpage_url":"https://example.com/sequence/one"},{"title":"Two","playlist_index":7,"webpage_url":"https://example.com/sequence/two"},{"title":"Three","playlist_index":11,"webpage_url":"https://example.com/sequence/three"}]}"#);
        } else if playlist {
            assert!(args.iter().any(|arg| arg == "--flat-playlist"));
            println!(
                "{}",
                r#"{"title":"Fixture playlist","_type":"playlist","entries":[{"title":"First: video"},null,{"title":"Broken video"},{"title":"Fourth video"}]}"#
            );
        } else if url.ends_with("/coalesced") && args.windows(2).any(|pair| pair == ["--add-header", "User-Agent:Probe Fixture"]) {
            println!("{}", r#"{"title":"Header-specific video"}"#);
        } else {
            println!(
                "{}",
                r#"{"title":"Fixture video","extractor":"fixture","formats":[{"format_id":"18","ext":"mp4","height":360,"vcodec":"avc1","acodec":"aac"}]}"#
            );
        }
    } else if args.iter().any(|arg| arg == "-format_whitelist") {
        std::process::exit(1);
    } else if args.iter().any(|arg| arg == "-progress") {
        let input = args.windows(2).find(|pair| pair[0] == "-i").unwrap()[1].as_str();
        let output = args.last().unwrap();
        assert!(!input.starts_with(r"\\?\") && !output.starts_with(r"\\?\"));
        if input.ends_with("media-000003-000002.mp4") {
            eprintln!("invalid fixture media");
            std::process::exit(1);
        }
        assert_eq!(
            args.iter().any(|arg| arg == "-c:s:0"),
            Path::new(input).with_extension("en.srt").is_file()
        );
        std::fs::copy(input, output).unwrap();
        println!("progress=end");
    } else if playlist {
        assert!(!std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .starts_with(r"\\?\"));
        assert!(args.iter().any(|arg| arg == "--ignore-errors"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-f" && pair[1].contains("[height<=720]")));
        assert!(args.windows(2).any(|pair| pair[0] == "-o"
            && pair[1] == "media-%(playlist_index,autonumber)06d-%(autonumber)06d.%(ext)s"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--merge-output-format" && pair[1] == "mp4/webm/mkv"));
        if url.ends_with("list=PL-links") {
            let index: u32 = args.windows(2).find(|pair| pair[0] == "--playlist-items").unwrap()[1].parse().unwrap();
            assert!(args.windows(2).any(|pair| pair == ["--add-header", "Origin:https://example.com"]));
            println!("SLIPSTREAM|4|4|4|4000|1|1|1|18");
            fs::write(format!("media-{index:06}-000001.mp4"), format!("item {index}")).unwrap();
            return;
        }
        if url.ends_with("list=PL6000") {
            assert!(!args.iter().any(|arg| arg == "--playlist-items"));
            fs::write("media-000001-000001.mp4", b"first").unwrap();
            fs::write("media-006000-006000.mp4", b"last").unwrap();
            return;
        }
        for (ordinal, (index, bytes)) in
            [(1, &b"first"[..]), (3, &b"broken"[..]), (4, &b"fourth"[..])]
                .into_iter()
                .enumerate()
        {
            let ordinal = ordinal + 1;
            let size = bytes.len();
            println!("SLIPSTREAM|{size}|{size}|NA|4000|1|{ordinal}|4|18");
            std::fs::write(format!("media-{index:06}-{ordinal:06}.mp4"), bytes).unwrap();
        }
        std::fs::write("media-000001-000001.en.srt", b"subtitle").unwrap();
        std::fs::write("media-000002.en.srt", b"unavailable").unwrap();
        std::fs::write("media-000002.srt", b"orphan subtitle").unwrap();
        std::fs::write("media-000002.mp4.part", b"partial").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        eprintln!("ERROR: video in position 2 is unavailable");
        std::process::exit(1);
    } else if let Some((_, name)) = url.split_once("/sequence/") {
        assert!(args.windows(2).any(|pair| pair[0] == "-f" && pair[1].contains("[height<=720]")));
        assert!(args.windows(2).any(|pair| pair == ["--add-header", "User-Agent:Playlist Fixture"]));
        let same_origin = url.starts_with("https://example.com/");
        for header in ["Origin:https://example.com", "Referer:https://example.com/playlist?list=PL-links&token=fixture"] {
            assert_eq!(args.windows(2).any(|pair| pair == ["--add-header", header]), same_origin);
        }
        log(dir, &format!("start {name}"));
        println!("SLIPSTREAM|2|4|4|4000|1");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !dir.join(format!("release-{name}")).exists() {
            assert!(Instant::now() < deadline, "timed out waiting for fixture release");
            sleep(Duration::from_millis(10));
        }
        log(dir, &format!("finish {name}"));
        if name == "two" {
            eprintln!("fixture playlist entry failed");
            std::process::exit(1);
        }
        fs::write("media-1.mp4", name).unwrap();
    } else {
        println!("SLIPSTREAM|2|4|4|4000|1");
        std::thread::sleep(std::time::Duration::from_millis(400));
        std::fs::write("media-1.mp4", b"test").unwrap();
    }
}
