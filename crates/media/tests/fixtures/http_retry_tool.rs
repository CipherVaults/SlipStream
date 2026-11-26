use std::{env, fs, io::Write, process};

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--version"] || args == ["-version"] {
        println!("2025.01.01");
        return;
    }

    let (url, args) = args.split_last().unwrap();
    let case = url.rsplit('/').next().unwrap();
    let probe = case.starts_with("probe-");
    assert_eq!(args.last().unwrap(), "--");
    assert_eq!(args.iter().any(|arg| arg == "--dump-single-json"), probe);
    assert_eq!(
        args.iter().any(|arg| arg == "--yes-playlist"),
        case == "download-playlist"
    );
    let user_agents: Vec<_> = args
        .windows(2)
        .filter(|pair| pair[0] == "--add-header")
        .filter_map(|pair| pair[1].strip_prefix("User-Agent:"))
        .collect();
    let expected_user_agent = if matches!(case, "probe-custom" | "download-non-403") {
        "Fixture browser"
    } else {
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
    };
    assert_eq!(user_agents, [expected_user_agent]);
    assert!(args
        .windows(2)
        .any(|pair| pair == ["--add-header", "Referer:https://example.com/watch"]));

    let ipv4 = args.iter().any(|arg| arg == "--force-ipv4");
    let executable = env::current_exe().unwrap();
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(executable.parent().unwrap().join(case))
        .unwrap();
    writeln!(log, "{}", if ipv4 { "ipv4" } else { "default" }).unwrap();

    let error = match case {
        "probe-retry" if !ipv4 => Some("HTTP Error 403"),
        "probe-always-forbidden" => Some("403: Forbidden"),
        "download-retry" | "download-playlist" | "download-partial" if !ipv4 => {
            Some("403: Forbidden")
        }
        "download-non-403" if !ipv4 => Some("HTTP Error 404: Not Found"),
        _ => None,
    };
    if let Some(error) = error {
        if case == "download-partial" {
            fs::write("media-000001.mp4.part", "partial output").unwrap();
        }
        eprintln!("ERROR: {error}");
        process::exit(1);
    }
    if probe {
        println!(r#"{{"title":"Fixture video"}}"#);
    } else {
        fs::write("media-000001.mp4", "fixture output").unwrap();
    }
}
