use std::{net::TcpListener, path::PathBuf};

pub struct ApiLaunch {
    pub port: u16,
    pub headless: bool,
    pub(crate) projects: Vec<super::scope::FrozenRoot>,
    pub(crate) imports: Vec<super::scope::FrozenRoot>,
    pub token: String,
    pub listener: TcpListener,
}
impl ApiLaunch {
    pub fn from_args() -> Result<Option<Self>, String> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        if !args.iter().any(|a| a == "--serve-api") {
            if args.iter().any(|a| a == "--headless") {
                return Err("--headless requires --serve-api".into());
            }
            return Ok(None);
        }
        let mut port = 34343;
        let mut headless = false;
        let mut projects = Vec::new();
        let mut imports = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--serve-api" => {}
                "--headless" => headless = true,
                "--api-port" | "--project-root" | "--import-root" => {
                    let flag = &args[i];
                    i += 1;
                    let value = args
                        .get(i)
                        .ok_or_else(|| format!("Missing value for {flag}"))?;
                    if flag == "--api-port" {
                        port = value.parse::<u16>().map_err(|_| "Invalid API port")?;
                        if port == 0 {
                            return Err("API port must be nonzero".into());
                        }
                    } else {
                        let p = PathBuf::from(value)
                            .canonicalize()
                            .map_err(|_| format!("Cannot resolve {flag}"))?;
                        if !p.is_dir() {
                            return Err(format!("{flag} must be a directory"));
                        }
                        if flag == "--project-root" {
                            projects
                                .push(super::scope::FrozenRoot::capture(&p).map_err(|e| e.message)?)
                        } else {
                            imports
                                .push(super::scope::FrozenRoot::capture(&p).map_err(|e| e.message)?)
                        }
                    }
                }
                arg => return Err(format!("Unknown API argument: {arg}")),
            }
            i += 1;
        }
        if projects.is_empty() {
            return Err("At least one --project-root is required".into());
        }
        let token = match std::env::var("LLM_WIKI_API_TOKEN") {
            Ok(token) => token,
            Err(_) => {
                let entry = keyring::Entry::new("com.llmwiki.desktop.local-api", "default")
                    .map_err(|_| "API credential store unavailable")?;
                match entry.get_password() {
                    Ok(token) => token,
                    Err(keyring::Error::NoEntry) => {
                        let token = format!(
                            "{}{}",
                            uuid::Uuid::new_v4().simple(),
                            uuid::Uuid::new_v4().simple()
                        );
                        entry
                            .set_password(&token)
                            .map_err(|_| "Could not store API credential")?;
                        token
                    }
                    Err(_) => return Err("Could not read API credential".into()),
                }
            }
        };
        // This is startup, before the async runtime is created. Child agents must not inherit API credentials.
        std::env::remove_var("LLM_WIKI_API_TOKEN");
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("API token must contain 64 hexadecimal characters".into());
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .map_err(|e| format!("API listener unavailable: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            port,
            headless,
            projects,
            imports,
            token,
            listener,
        }))
    }
}

pub fn handle_token_command() -> bool {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--api-token"] {
        return false;
    }
    match keyring::Entry::new("com.llmwiki.desktop.local-api", "default")
        .and_then(|entry| entry.get_password())
    {
        Ok(token) => println!("{token}"),
        Err(_) => eprintln!("Local API credential unavailable. Start the API host first."),
    }
    true
}
