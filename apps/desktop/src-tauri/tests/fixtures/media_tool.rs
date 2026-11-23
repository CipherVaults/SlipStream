fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let playlist = args.iter().any(|arg| arg == "--yes-playlist");
    if let Some(pair) = args.windows(2).find(|pair| pair[0] == "--ffmpeg-location") {
        assert!(!pair[1].starts_with(r"\\?\"));
    }
    if args.iter().any(|arg| arg == "--version") {
        println!("yt-dlp fixture 1.0");
    } else if args.iter().any(|arg| arg == "-version") {
        println!("ffmpeg fixture 1.0");
    } else if args.iter().any(|arg| arg == "--dump-single-json") {
        if playlist && args.last().is_some_and(|url| url.ends_with("list=PL6000")) {
            let entry = format!(r#"{{"title":"{}"}}"#, "Title ".repeat(100));
            let entries = vec![entry.as_str(); 6000].join(",");
            println!(r#"{{"_type":"playlist","entries":[{entries}]}}"#);
        } else if playlist {
            assert!(args.iter().any(|arg| arg == "--flat-playlist"));
            println!(
                "{}",
                r#"{"title":"Fixture playlist","_type":"playlist","entries":[{"title":"First: video"},null,{"title":"Broken video"},{"title":"Fourth video"}]}"#
            );
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
            input.ends_with("media-000001-000001.mp4")
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
    } else {
        println!("SLIPSTREAM|2|4|4|4000|1");
        std::thread::sleep(std::time::Duration::from_millis(400));
        std::fs::write("media-1.mp4", b"test").unwrap();
    }
}
