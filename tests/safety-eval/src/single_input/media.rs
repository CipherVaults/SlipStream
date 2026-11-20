use super::{accepted, rejected, require, Suite, CDN_URL, TITLES};
use slipstream_media::args::{
    validate_ffmpeg_audio_filter, validate_ffmpeg_flag, validate_ytdlp_flag, BuildFfmpegArgs,
    BuildYtDlpArgs,
};
use slipstream_protocol::MediaContainer;
use std::{ffi::OsString, path::Path};

const CATEGORY: &str = "media_sidecar";

pub(super) fn run(suite: &mut Suite) {
    for (surface, inputs) in [
        (
            "ytdlp_flag",
            &[
                "--exec=calc",
                "--exec",
                "--exec-before-download",
                "--batch-file",
                "--batch-file=fixture.txt",
                "--config-locations",
                "--config-locations=fixture.conf",
                "--ppa",
                "--ppa=postprocessor",
                "--postprocessor-args",
                "--external-downloader",
                "--external-downloader-args",
                "--load-info-json",
                "--cookies",
                "--cookies-from-browser",
                "--netrc",
                "--proxy",
                "--socket-timeout=0",
                "--paths",
                "--downloader",
                "--print-to-file",
                "--enable-file-urls",
                "--use-postprocessor",
                "--plugin-dirs",
            ][..],
        ),
        (
            "ytdlp_template",
            &[
                "-o $(fixture)",
                "$(calc).mp4",
                "clip\u{0060}fixture\u{0060}.mp4",
                "../outside.mp4",
                "%(title)s",
                "CON.mp4",
                ":stream",
                "preview\u{202e}4pm.txt",
                "clips\\outside.mp4",
                "capture..old.mp4",
                "100%.mp4",
                "asset:Zone.Identifier",
                "AUX.mkv",
                "file\0.mp4",
                "bad|name.mp4",
                "clip\u{2066}.mp4",
                "-payload.mp4",
                "%(uploader)s.mov",
            ][..],
        ),
        (
            "ytdlp_url",
            &[
                "file:///fixture/clip.mp4",
                "smb://files.example.test/clip.mp4",
                "javascript:void(0)",
                "-x",
                "ftp://files.example.test/clip",
                "data:text/plain,fixture",
                "gopher://files.example.test/fixture",
                "https://cdn.example.test/c\nlip.mp4",
                "magnet:?xt=urn:btih:fixture",
                "https://cdn.example.test/clip\0",
            ][..],
        ),
        (
            "ytdlp_format",
            &[
                "-137",
                "137;calc",
                "best audio",
                "best[height=720]",
                "137&&140",
                "137|140",
                "137++140",
                "137/",
                "/140",
                "137+-140",
                "137/<140",
                "$(fixture)",
                "137\n140",
                "137%140",
                "'137'",
                "bestvideo+bestaudio/-best",
            ][..],
        ),
        (
            "ffmpeg_flag",
            &[
                "-filter_complex",
                "-lavfi",
                "-filter_script",
                "-filter_complex_script",
                "-protocol_whitelist",
                "-protocol_blacklist",
                "-safe",
                "-attach",
                "-dump_attachment:t:0",
                "-passlogfile",
                "-report",
                "-f=concat",
            ][..],
        ),
        (
            "ffmpeg_audio_filter",
            &[
                "amovie=fixture.wav",
                "amovie=other.wav",
                "movie=fixture.mp4",
                "concat=n=2:v=0:a=1",
                "loudnorm,amovie=fixture.wav",
                "volume=1;amovie=fixture.wav",
                "loudnorm=I=-15:TP=-1.5:LRA=11",
                "loudnorm=I=-16:TP=-1.5:LRA=12",
                "afade=t=in",
                "asendcmd=f=fixture.txt",
                "afir=irfmt=input",
                "[0:a]loudnorm",
            ][..],
        ),
        (
            "ffmpeg_input",
            &[
                "concat:clip.mp4",
                "file:///fixture/clip.mp4",
                "tcp:fixture",
                "playlist.m3u8",
                "clip.ffconcat",
                "../clip.mp4",
                "CON.mp4",
                ":stream",
                "http://cdn.example.test/clip.mp4",
                "crypto:clip.mp4",
                "\\\\files\\share\\clip.mp4",
                "//files/share/clip.mp4",
                "-clip.mp4",
                "fixture/../clip.mp4",
                "clip.mp4:ads",
                "preview\u{202e}4pm.mp4",
                "clip\0.mp4",
                "playlist.m3u",
                "capture.unknown",
                "AUX.mkv",
            ][..],
        ),
        (
            "ffmpeg_output",
            &[
                "../final.mp4",
                "NUL.mp4",
                "out:stream.mp4",
                "out\u{2069}.mp4",
                "out?.mp4",
                "pipe:1",
                "\\\\fixtures\\share\\out.mp4",
                "-out.mp4",
            ][..],
        ),
    ] {
        for raw in inputs {
            let item = suite.register(
                CATEGORY,
                "harmful",
                surface,
                "direct",
                format!("{surface}\0{raw}").into_bytes(),
            );
            suite.grade(item, || {
                match surface {
                    "ytdlp_flag" => rejected(validate_ytdlp_flag(raw), surface)?,
                    "ytdlp_template" => rejected(
                        BuildYtDlpArgs::download(false).output_template(raw),
                        surface,
                    )?,
                    "ytdlp_url" => rejected(BuildYtDlpArgs::download(false).build(raw), surface)?,
                    "ytdlp_format" => {
                        rejected(BuildYtDlpArgs::download(false).format(raw), surface)?
                    }
                    "ffmpeg_flag" => rejected(validate_ffmpeg_flag(raw), surface)?,
                    "ffmpeg_audio_filter" => rejected(validate_ffmpeg_audio_filter(raw), surface)?,
                    "ffmpeg_input" => {
                        rejected(BuildFfmpegArgs::new().local_input(Path::new(raw)), surface)?
                    }
                    "ffmpeg_output" => {
                        let builder = accepted(
                            BuildFfmpegArgs::new().local_input(Path::new("fixture.mp4")),
                            "FFmpeg control input",
                        )?;
                        rejected(builder.build(Path::new(raw), MediaContainer::Mp4), surface)?;
                    }
                    _ => unreachable!("unknown sidecar surface {surface}"),
                }
                Ok("The sidecar argument validator rejected the unsafe value.")
            });
        }
    }

    let formats = [
        "2160p60+bestaudio",
        "137+140",
        "bestvideo+bestaudio/best",
        "251",
        "audio-opus.1",
        "22",
    ];
    for title in TITLES {
        for (index, format) in formats.into_iter().enumerate() {
            for playlist in [false, true] {
                let (ext, demuxer, container, muxer) = [
                    ("mp4", "mov", MediaContainer::Mp4, "mp4"),
                    ("mkv", "matroska", MediaContainer::Mkv, "matroska"),
                    ("webm", "matroska", MediaContainer::Webm, "webm"),
                    ("m4a", "mov", MediaContainer::M4a, "ipod"),
                    ("flac", "flac", MediaContainer::Flac, "flac"),
                    ("wav", "wav", MediaContainer::Wav, "wav"),
                ][index];
                let url = format!("{CDN_URL}?item=[123]&name=clip%20preview&sig=a%2Fb%3D%3D&list={playlist}&format={index}");
                let name = format!("{title}.{ext}");
                let output = format!("{title} (normalized).{ext}");
                let input = format!("{url}\0{format}\0{name}\0{output}");
                let item = suite.register(
                    CATEGORY,
                    "benign",
                    "sidecar_argv",
                    "composed",
                    input.into_bytes(),
                );
                suite.grade(item, || {
                    let yt = accepted(
                        BuildYtDlpArgs::download(playlist)
                            .format(format)
                            .and_then(|b| b.output_template(&name))
                            .and_then(|b| b.build(&url)),
                        "yt-dlp builder",
                    )?;
                    require(
                        has_args(&yt, &["-f", format])
                            && has_args(&yt, &["-o", &name])
                            && has_args(&yt, &[if playlist { "--yes-playlist" } else { "--no-playlist" }])
                            && ends_with(&yt, &["--", &url]),
                        "yt-dlp did not preserve discrete format, template and URL arguments",
                    )?;
                    let ff = accepted(
                        BuildFfmpegArgs::new()
                            .local_input(Path::new(&name))
                            .and_then(|b| b.normalize_audio().build(Path::new(&output), container)),
                        "FFmpeg builder",
                    )?;
                    require(
                        has_args(&ff, &["-f", demuxer, "-i", &name])
                            && has_args(&ff, &["-af", "loudnorm=I=-16:TP=-1.5:LRA=11"])
                            && ends_with(&ff, &["-f", muxer, "--", &output]),
                        "FFmpeg did not preserve discrete demuxer, input, filter and output arguments",
                    )?;
                    Ok("Both sidecar builders preserved discrete arguments and option boundaries.")
                });
            }
        }
    }
}

fn has_args(args: &[OsString], sequence: &[&str]) -> bool {
    args.windows(sequence.len())
        .any(|window| matches_args(window, sequence))
}

fn ends_with(args: &[OsString], sequence: &[&str]) -> bool {
    args.len() >= sequence.len() && matches_args(&args[args.len() - sequence.len()..], sequence)
}

fn matches_args(args: &[OsString], sequence: &[&str]) -> bool {
    args.iter()
        .zip(sequence)
        .all(|(actual, expected)| actual == expected)
}
