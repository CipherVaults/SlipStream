fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("yt-dlp fixture 1.0");
    } else if args.iter().any(|arg| arg == "-version") {
        println!("ffmpeg fixture 1.0");
    } else if args.iter().any(|arg| arg == "--dump-single-json") {
        println!(
            "{}",
            r#"{"title":"Fixture video","extractor":"fixture","formats":[{"format_id":"18","ext":"mp4","height":360,"vcodec":"avc1","acodec":"aac"}]}"#
        );
    } else {
        println!("SLIPSTREAM|2|4|4|4000|1");
        std::thread::sleep(std::time::Duration::from_millis(400));
        std::fs::write("media-1.mp4", b"test").unwrap();
    }
}
