#[cfg(windows)]
mod windows_main {
    use slipstream_native_host::{
        manifest::{self, Browser, DEFAULT_FIREFOX_ID},
        registry::{self, DEFAULT_BASE_KEY},
        security::{default_pipe_name, validate_local_pipe},
        stdio,
        token::{default_token_path, local_app_directory},
    };
    use std::{
        env,
        ffi::OsString,
        io,
        path::{Path, PathBuf},
    };

    const HELP: &str = "\
slipstream-native-host [--stdio | --install-hosts | --uninstall-hosts | --print-manifests]
  --chrome-origin <origin>    Chromium extension origin (repeat for multiple IDs)
  --firefox-extension <id>    Firefox add-on ID (default slipstream@ciphervaults.com)
  --exe-path <path>           Executable path for generated manifests
  --pipe-name <name>          Override the SID-derived local pipe name
  --token-file <path>         Override the session token file
  --help                      Show help
  --version                   Show version";

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }

    #[derive(Clone, Copy)]
    enum Mode {
        Stdio,
        Install,
        Uninstall,
        Print,
        Help,
        Version,
    }

    fn browser_client(args: &[OsString]) -> Option<String> {
        let first = args.first()?;
        if let Some(origin) = first
            .to_str()
            .and_then(|origin| manifest::chromium_origin(origin).ok())
        {
            return Some(origin);
        }
        let path = Path::new(first);
        if path.is_absolute()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            return Some(
                args.get(1)
                    .and_then(|id| id.to_str())
                    .filter(|id| manifest::valid_firefox_id(id))
                    .unwrap_or("firefox")
                    .into(),
            );
        }
        None
    }

    fn run_stdio(pipe: Option<String>, token: Option<PathBuf>, client: &str) -> io::Result<()> {
        let pipe = pipe.map_or_else(default_pipe_name, Ok)?;
        validate_local_pipe(&pipe)?;
        let token = token.map_or_else(default_token_path, Ok)?;
        stdio::forward(
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
            &pipe,
            &token,
            client,
        )
    }

    pub fn run() -> io::Result<()> {
        let args = env::args_os().skip(1).collect::<Vec<_>>();
        if let Some(client) = browser_client(&args) {
            return run_stdio(None, None, &client);
        }

        let mut mode = None;
        let mut origins = Vec::new();
        let mut firefox_ids = Vec::new();
        let mut executable = None;
        let mut pipe = None;
        let mut token = None;
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            let mut value = || args.next().ok_or_else(|| invalid("missing option value"));
            let mut string = || {
                value()?
                    .into_string()
                    .map_err(|_| invalid("expected Unicode option value"))
            };
            let selected = match argument.to_str() {
                Some("--stdio") => Mode::Stdio,
                Some("--install-hosts") => Mode::Install,
                Some("--uninstall-hosts") => Mode::Uninstall,
                Some("--print-manifests") => Mode::Print,
                Some("--help") => Mode::Help,
                Some("--version") => Mode::Version,
                Some("--chrome-origin") => {
                    origins.push(manifest::chromium_origin(&string()?)?);
                    continue;
                }
                Some("--firefox-extension") => {
                    let id = string()?;
                    if !manifest::valid_firefox_id(&id) {
                        return Err(invalid("invalid Firefox extension ID"));
                    }
                    firefox_ids.push(id);
                    continue;
                }
                Some("--exe-path") => {
                    executable = Some(PathBuf::from(value()?));
                    continue;
                }
                Some("--pipe-name") => {
                    pipe = Some(string()?);
                    continue;
                }
                Some("--token-file") => {
                    token = Some(PathBuf::from(value()?));
                    continue;
                }
                _ => return Err(invalid("unknown option (see --help)")),
            };
            if mode.replace(selected).is_some() {
                return Err(invalid("choose one operation (see --help)"));
            }
        }

        match mode.unwrap_or(Mode::Help) {
            Mode::Help => println!("{HELP}"),
            Mode::Version => println!(
                "slipstream-native-host {}",
                slipstream_native_host::native_host_version()
            ),
            Mode::Stdio => run_stdio(pipe, token, "browser-stdio")?,
            Mode::Uninstall => registry::uninstall_hosts(DEFAULT_BASE_KEY)?,
            mode @ (Mode::Install | Mode::Print) => {
                if firefox_ids.is_empty() {
                    firefox_ids.push(DEFAULT_FIREFOX_ID.to_owned());
                }
                let exe = executable.map_or_else(env::current_exe, std::path::absolute)?;
                if matches!(mode, Mode::Install) {
                    let directory = local_app_directory()?.join("NativeMessagingHosts");
                    for path in registry::install_hosts(
                        DEFAULT_BASE_KEY,
                        &directory,
                        &exe,
                        &origins,
                        &firefox_ids,
                    )? {
                        println!("{}", path.display());
                    }
                } else {
                    let mut manifests = serde_json::Map::new();
                    for browser in Browser::ALL {
                        let manifest = manifest::generate(browser, &exe, &origins, &firefox_ids)?;
                        manifests
                            .insert(browser.directory().into(), serde_json::to_value(manifest)?);
                    }
                    println!("{}", serde_json::to_string_pretty(&manifests)?);
                }
            }
        }
        Ok(())
    }
}

fn main() {
    #[cfg(windows)]
    if let Err(error) = windows_main::run() {
        eprintln!("slipstream-native-host: {error}");
        std::process::exit(1);
    }

    #[cfg(not(windows))]
    eprintln!("slipstream-native-host requires Windows");
}
