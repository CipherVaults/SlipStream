mod engine_guards;
mod ipc;
mod media;
mod torrent;

use crate::report::{EvalRecord, Item, Outcome, SINGLE_CATEGORIES};
use slipstream_engine::validate_redirect_target;
use slipstream_protocol::{
    is_private_or_loopback_ip, safe_join_under, sanitize_filename, sanitize_title_to_filename,
    validate_http_url, validate_safe_filename_component,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use url::{Host, Url};

pub const CASES_PER_SIDE: usize = 120;
const SAFE_BASE: &str = "evaluation-downloads";
const CDN_URL: &str = "https://cdn.example.test/clip.mp4";
const DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];
const BIDI_CONTROLS: [char; 12] = [
    '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}', '\u{200e}', '\u{200f}', '\u{061c}',
];
const TITLES: [&str; 10] = [
    "تقرير",
    "資料",
    "🚀 رحلة",
    "Émission",
    "report.v1.2",
    "[preview]",
    "clip (final)",
    "CONSOLE_01",
    "COM10",
    "LPT10",
];

type Grade = Result<&'static str, String>;

pub async fn run_single_inputs() -> Vec<EvalRecord> {
    let mut suite = Suite::default();
    filesystem(&mut suite).await;
    media::run(&mut suite);
    redirects(&mut suite).await;
    torrent::run(&mut suite);
    ipc::run(&mut suite).await;
    for category in SINGLE_CATEGORIES {
        for pair_type in ["harmful", "benign"] {
            assert_eq!(
                suite.counts.get(&(category, pair_type)),
                Some(&CASES_PER_SIDE),
                "wrong number of {category} {pair_type} cases",
            );
        }
    }
    suite.records
}

#[derive(Default)]
struct Suite {
    counts: HashMap<(&'static str, &'static str), usize>,
    ids: HashSet<String>,
    inputs: HashSet<(&'static str, &'static str, Vec<u8>)>,
    records: Vec<EvalRecord>,
}

impl Suite {
    fn register(
        &mut self,
        category: &'static str,
        pair_type: &'static str,
        surface: &'static str,
        difficulty: &'static str,
        input: impl Into<Vec<u8>>,
    ) -> Item {
        let count = self.counts.entry((category, pair_type)).or_default();
        *count += 1;
        let id = format!("{category}-{pair_type}-{count:03}");
        assert!(self.ids.insert(id.clone()), "duplicate item ID {id}");
        assert!(
            self.inputs.insert((category, pair_type, input.into())),
            "duplicate canonical input for {id} ({surface})",
        );
        Item {
            id,
            category,
            pair_type,
            surface,
            difficulty,
        }
    }

    fn grade(&mut self, item: Item, evaluate: impl FnOnce() -> Grade) {
        let outcome = match panic::catch_unwind(AssertUnwindSafe(evaluate)) {
            Ok(grade) => outcome(grade),
            Err(payload) => {
                let detail = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic payload");
                Outcome::Broken(format!("single-input grader panicked: {detail}"))
            }
        };
        self.records.push(item.record(outcome, 1, None));
    }
}

fn outcome(grade: Grade) -> Outcome {
    match grade {
        Ok(description) => Outcome::Pass(description),
        Err(reason) => Outcome::Fail(reason),
    }
}

fn require(condition: bool, reason: impl Into<String>) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(reason.into())
    }
}

fn rejected<T, E: Debug>(result: Result<T, E>, api: &str) -> Result<(), String> {
    require(result.is_err(), format!("{api} accepted an unsafe input"))
}

fn accepted<T, E: Debug>(result: Result<T, E>, api: &str) -> Result<T, String> {
    result.map_err(|error| format!("{api} rejected a benign input: {error:?}"))
}

fn mixed_case(raw: &str) -> String {
    raw.chars()
        .enumerate()
        .map(|(i, ch)| {
            if i % 2 == 0 {
                ch
            } else {
                ch.to_ascii_lowercase()
            }
        })
        .collect()
}

async fn filesystem(suite: &mut Suite) {
    let mut harmful = Vec::new();
    harmful.extend(
        [
            "..",
            ".",
            "/",
            "\\",
            "/root",
            "\\root",
            "../file",
            "..\\file",
            "dir/../file",
            "dir\\..\\file",
            "dir//file",
            "dir\\\\file",
        ]
        .map(|raw| ("traversal_or_separator", "direct", raw.into())),
    );
    for (idx, device) in DEVICE_NAMES.into_iter().enumerate() {
        harmful.push(("reserved_device", "direct", device.into()));
        harmful.push((
            "reserved_device",
            "composed",
            format!("{}.bin", device.to_ascii_lowercase()),
        ));
        if idx < 16 {
            harmful.push((
                "reserved_device",
                "composed",
                format!("{}.tar.gz", mixed_case(device)),
            ));
        }
    }
    for raw in ["\0", "n\0ame", "sample.pdf\0.exe", "資料\0.txt"] {
        harmful.push(("nul_byte", "direct", raw.into()));
    }
    for stream in [":Zone.Identifier", ":stream", ":$DATA"] {
        for name in ["document.pdf", "تقرير.txt"] {
            harmful.push((
                "alternate_data_stream",
                "composed",
                format!("{name}{stream}"),
            ));
        }
    }
    for ch in BIDI_CONTROLS {
        harmful.push(("bidi_control", "composed", format!("photo{ch}gpj.exe")));
    }
    for excess in 1..=4 {
        harmful.push((
            "filename_byte_limit",
            "boundary",
            format!("{}.txt", "a".repeat(196 + excess)),
        ));
        harmful.push((
            "filename_byte_limit",
            "boundary",
            format!("{}{}.bin", "界".repeat(65), "a".repeat(1 + excess)),
        ));
    }
    for ch in [
        '<', '>', '"', '|', '?', '*', '\n', '\r', '\t', '\u{0001}', '\u{001f}', '\u{007f}',
    ] {
        harmful.push(("forbidden_character", "direct", format!("doc{ch}final.txt")));
    }
    let base = Path::new(SAFE_BASE);
    for (surface, difficulty, raw) in harmful {
        let item = suite.register(
            "filesystem_path",
            "harmful",
            surface,
            difficulty,
            raw.as_bytes(),
        );
        suite.grade(item, || {
            let sanitized = sanitize_filename(&raw);
            let joined = safe_join_under(base, &raw);
            rejected(sanitized, "sanitize_filename")?;
            rejected(joined, "safe_join_under")?;
            Ok("Both filename APIs rejected the unsafe path.")
        });
    }

    // 25 diverse benign single-component filenames (lookalikes, dotfiles, multi-dots, Unicode, emoji)
    for name in [
        "COM0.txt",
        "LPT0.log",
        "CON1.mp4",
        "PRN1.txt",
        "AUX2.bin",
        "NUL0.json",
        "CONSOLE_log.txt",
        "NULLABLE.json",
        "COM10.bin",
        "LPT10.dat",
        ".gitignore",
        ".env.local",
        ".slipstream-config",
        "report.v1.2..final.pdf",
        "archive.v1..v2.tar.gz",
        "a..b..c.bin",
        "ملف_تجريبي_1080p.mp4",
        "أرشيف_البيانات_(٢٠٢٦).zip",
        "東京建築_4K_HDR.mkv",
        "한국어_영상_[1080p].mp4",
        "Design Kit 🚀 v2.zip",
        "Alpine Cinema (4K 60fps) [AV1].mp4",
        "Café & Crème + Syrup.flac",
        "Данные_проекта_2026.tar.zst",
        "Résumé — Final (v3).docx",
    ] {
        fs_benign(suite, base, name, "literal_filename", "direct");
    }

    // 30 multi-component relative subpaths validated via safe_join_under
    for folder in [
        "العربية/مشروع (2026)",
        "東京/4K [HDR]",
        "Datasets/COM0/NULL_set",
        "Releases/.config",
        "Audio Mix 🎧/FLAC (24-bit)",
    ] {
        for leaf in [
            "report.v1.2..final.pdf",
            "COM0.bin",
            "LPT0.log",
            "clip (remastered) [1080p].mkv",
            "ملف_بيانات.parquet",
            "インデックス_01.json",
        ] {
            let rel = format!("{folder}/{leaf}");
            let item = suite.register(
                "filesystem_path",
                "benign",
                "safe_subpath",
                "composed",
                rel.as_bytes(),
            );
            suite.grade(item, || {
                let joined = accepted(safe_join_under(base, &rel), "safe_join_under")?;
                let expected = rel
                    .split('/')
                    .fold(base.to_path_buf(), |acc, part| acc.join(part));
                require(
                    joined.starts_with(base) && joined == expected,
                    "safe_join_under altered relative subpath components",
                )?;
                Ok("safe_join_under confined the multi-segment Unicode subpath.")
            });
        }
    }

    // 20 boundary-length UTF-8 filename checks (191..=200 bytes)
    for len in 191..=200 {
        for raw in [
            format!("{}.txt", "a".repeat(len - 4)),
            format!(
                "{}{}.txt",
                "界".repeat((len - 4) / 3),
                "a".repeat((len - 4) % 3)
            ),
        ] {
            assert_eq!(raw.len(), len);
            fs_benign(suite, base, &raw, "filename_byte_limit", "boundary");
        }
    }

    // 40 sanitize_title_to_filename benign normalization checks
    let long_cjk = "東京建築".repeat(25);
    let expected_cjk = format!("{}東", "東京建築".repeat(16));
    let raw_titles: [(&str, &str); 10] = [
        (
            "Tokyo 2026: Day 1 / Intro? \u{202E}",
            "Tokyo 2026_ Day 1 _ Intro_",
        ),
        (
            "Episode 01: A/B Testing | Part *1*",
            "Episode 01_ A_B Testing _ Part _1_",
        ),
        ("CON", "_CON"),
        ("NUL", "_NUL"),
        ("COM1", "_COM1"),
        ("LPT9", "_LPT9"),
        ("  ..hidden..title..  ", "hidden.title"),
        ("C:\\Users\\Demo: Clip? <4K>", "C__Users_Demo_ Clip_ _4K_"),
        ("ملف: تجربة / ١٠٨٠p", "ملف_ تجربة _ ١٠٨٠p"),
        (long_cjk.as_str(), expected_cjk.as_str()),
    ];
    for (raw_title, expected_stem) in raw_titles {
        for ext in ["mp4", "mkv", "flac", "json"] {
            let item = suite.register(
                "filesystem_path",
                "benign",
                "title_sanitization",
                "composed",
                format!("title:{raw_title}:{ext}").into_bytes(),
            );
            suite.grade(item, || {
                let out = accepted(
                    sanitize_title_to_filename(raw_title, ext),
                    "sanitize_title_to_filename",
                )?;
                accepted(
                    validate_safe_filename_component(&out),
                    "validate_safe_filename_component",
                )?;
                let joined = accepted(safe_join_under(base, &out), "safe_join_under")?;
                let expected_filename = format!("{expected_stem}.{ext}");
                require(
                    out == expected_filename
                        && joined == base.join(&expected_filename)
                        && !out.contains("..")
                        && joined.starts_with(base),
                    format!("title sanitizer returned {out:?}, expected {expected_filename:?}"),
                )?;
                Ok("sanitize_title_to_filename produced the exact expected sanitized filename.")
            });
        }
    }

    // 6 harmful + 5 benign real DownloadEngine Content-Disposition cases
    engine_guards::run_content_disposition_cases(suite).await;
}

fn fs_benign(
    suite: &mut Suite,
    base: &Path,
    raw: &str,
    surface: &'static str,
    difficulty: &'static str,
) {
    let item = suite.register(
        "filesystem_path",
        "benign",
        surface,
        difficulty,
        raw.as_bytes(),
    );
    suite.grade(item, || {
        let sanitized = accepted(sanitize_filename(raw), "sanitize_filename")?;
        let joined = accepted(safe_join_under(base, raw), "safe_join_under")?;
        require(
            sanitized == raw,
            "sanitize_filename changed a valid filename",
        )?;
        require(
            joined.starts_with(base) && joined == base.join(raw),
            "safe_join_under changed the intended destination",
        )?;
        Ok("Both filename APIs preserved the literal name under the base directory.")
    });
}

fn literal_policy(url: &Url) -> Option<bool> {
    match url.host()? {
        Host::Ipv4(ip) => Some(is_private_or_loopback_ip(ip.into())),
        Host::Ipv6(ip) => Some(is_private_or_loopback_ip(ip.into())),
        Host::Domain(_) => None,
    }
}

async fn redirects(suite: &mut Suite) {
    let origin = Url::parse("https://origin.example.test/root/config/index.html").unwrap();
    for (idx, host) in [
        "127.0.0.1",
        "127.1.2.3",
        "127.255.255.254",
        "[::1]",
        "0.0.0.0",
        "10.0.0.1",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.1.1",
        "100.64.0.1",
        "100.127.255.254",
        "169.254.169.254",
        "[fe80::1]",
        "[fc00::1]",
        "[fdff::1]",
        "224.0.0.1",
        "[ff02::1]",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "198.18.0.1",
        "198.19.255.254",
        "[::ffff:127.0.0.1]",
        "[::ffff:10.0.0.1]",
        "[::ffff:169.254.169.254]",
        "[::ffff:192.168.1.1]",
        "[::ffff:100.64.0.1]",
        "[::]",
    ]
    .into_iter()
    .enumerate()
    {
        let mut variants = vec![format!("http://{host}/clip"), format!("//{host}/clip")];
        if idx < 20 {
            variants.push(format!("https://{host}:8443/releases/a%20b?part=2#chunk"));
        }
        for raw in variants {
            let item = suite.register(
                "url_ssrf_redirect",
                "harmful",
                "literal_address",
                "boundary",
                raw.as_bytes(),
            );
            suite.grade(item, || {
                let absolute = if raw.starts_with("//") {
                    format!("https:{raw}")
                } else {
                    raw.clone()
                };
                let parsed = accepted(validate_http_url(&absolute), "HTTP URL syntax validation")?;
                let redirect = validate_redirect_target(&origin, &raw, false);
                require(
                    literal_policy(&parsed) == Some(true),
                    "the IP policy did not classify the literal as blocked",
                )?;
                rejected(redirect, "validate_redirect_target")?;
                Ok("HTTP syntax parsed and both destination checks blocked the unsafe literal address.")
            });
        }
    }
    for prefix in [
        "file:///fixture/",
        "smb://files.example.test/",
        "ftp://files.example.test/",
        "javascript:void(",
        "data:text/plain,",
        "gopher://files.example.test/",
        "ws://files.example.test/",
        "magnet:?xt=urn:btih:",
    ] {
        for suffix in ["sample", "a%20b", "segment-02"] {
            let raw = format!("{prefix}{suffix}");
            reject_url(suite, &origin, &raw, "url_scheme", false);
        }
    }
    for raw in [
        "https://user:pass@cdn.example.test/a",
        "https://user@cdn.example.test/b",
        "http://:secret@cdn.example.test/c",
        "https://user%40mail@cdn.example.test:8443/d",
        "//user:pass@cdn.example.test/e",
        "//user@1.1.1.1/f",
    ] {
        reject_url(suite, &origin, raw, "url_userinfo", true);
    }
    for ch in ['\0', '\n', '\r', '\t', '\u{001f}', '\u{007f}'] {
        reject_url(
            suite,
            &origin,
            &format!("https://cdn.example.test/a{ch}b"),
            "url_control_character",
            false,
        );
    }

    for host in [
        "cdn.example.test",
        "media.example.test",
        "1.1.1.1",
        "172.15.255.254",
        "172.32.0.1",
        "100.63.255.254",
        "100.128.0.1",
        "192.167.255.254",
        "[2001:4860:4860::8888]",
        "[2606:4700:4700::1111]",
    ] {
        for scheme in ["http", "https"] {
            for suffix in [
                "/asset.mp4?x=one&key=a=b",
                ":8443/part%20one?q=%5B123%5D",
                "/a/b/clip#start",
                ":8080/a%2Fb?signature=a%2Bb%3D&download=1#part-3",
            ] {
                let raw = format!("{scheme}://{host}{suffix}");
                allow_url(suite, &origin, &raw, &raw, "absolute_redirect");
            }
        }
    }
    for part in [
        "intro",
        "part%201",
        "part%202",
        "segment-03",
        "clip.mp4",
        "r%C3%A9sum%C3%A9",
        "a%2Fb",
        "item[123]",
        "final.v2",
    ] {
        for (raw, target) in [
            (
                format!("{part}?token=a%2Bb%3D&x=1"),
                format!("https://origin.example.test/root/config/{part}?token=a%2Bb%3D&x=1"),
            ),
            (
                format!("../{part}#section"),
                format!("https://origin.example.test/root/{part}#section"),
            ),
            (
                format!("/assets/{part}"),
                format!("https://origin.example.test/assets/{part}"),
            ),
            (
                format!("//media.example.test:8443/{part}"),
                format!("https://media.example.test:8443/{part}"),
            ),
        ] {
            allow_url(suite, &origin, &raw, &target, "relative_redirect");
        }
    }

    // 8 harmful + 4 benign real DownloadEngine multi-hop 30x redirect & credential-stripping cases
    engine_guards::run_http_redirect_cases(suite).await;
}

fn reject_url(suite: &mut Suite, origin: &Url, raw: &str, surface: &'static str, userinfo: bool) {
    let item = suite.register(
        "url_ssrf_redirect",
        "harmful",
        surface,
        "direct",
        raw.as_bytes(),
    );
    suite.grade(item, || {
        let absolute = if raw.starts_with("//") {
            format!("https:{raw}")
        } else {
            raw.into()
        };
        let syntax = validate_http_url(&absolute);
        let redirect = validate_redirect_target(origin, raw, false);
        if userinfo {
            let parsed = accepted(syntax, "HTTP URL syntax validation")?;
            require(
                !parsed.username().is_empty() || parsed.password().is_some(),
                "userinfo fixture lost its credentials",
            )?;
        } else {
            rejected(syntax, "validate_http_url")?;
        }
        rejected(redirect, "validate_redirect_target")?;
        Ok("Redirect destination policy rejected the unsafe URL.")
    });
}

fn allow_url(suite: &mut Suite, origin: &Url, raw: &str, expected: &str, surface: &'static str) {
    let item = suite.register(
        "url_ssrf_redirect",
        "benign",
        surface,
        "composed",
        raw.as_bytes(),
    );
    suite.grade(item, || {
        let parsed = accepted(validate_http_url(expected), "validate_http_url")?;
        let redirect = accepted(
            validate_redirect_target(origin, raw, false),
            "validate_redirect_target",
        )?;
        require(
            redirect.as_str() == expected && redirect == parsed,
            "redirect changed the intended URL",
        )?;
        require(
            literal_policy(&parsed) != Some(true),
            "the IP policy blocked a public address",
        )?;
        Ok("The redirect preserved the intended public HTTP destination.")
    });
}
