#[macro_use] extern crate serde_json;

use futures_util::{future, FutureExt, StreamExt};
use librespot_playback::player::PlayerEvent;
use log::{error, info, warn};
use sha1::{Digest, Sha1};
use tokio::sync::mpsc::UnboundedReceiver;
use url::Url;

use librespot::connect::spirc::Spirc;
use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::{ConnectConfig, DeviceType, SessionConfig};
use librespot::core::session::Session;
use librespot::core::version;
use librespot::playback::audio_backend::{self, SinkBuilder};
use librespot::playback::config::{
    AudioFormat, Bitrate, NormalisationMethod, NormalisationType, PlayerConfig, VolumeCtrl,
};
use librespot::playback::mixer::{self, MixerConfig, MixerFn};
use librespot::playback::player::Player;

mod spotty;
use spotty::{LMS};

use std::process::exit;
use std::str::FromStr;
use std::{env, time::Instant};
use std::{
    io::{stderr, Write},
    pin::Pin,
};

const VERSION: &'static str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));

#[cfg(target_os="windows")]
const NULLDEVICE: &'static str = "NUL";
#[cfg(not(target_os="windows"))]
const NULLDEVICE: &'static str = "/dev/null";

fn device_id(name: &str) -> String {
    hex::encode(Sha1::digest(name.as_bytes()))
}

fn usage(program: &str, opts: &getopts::Options) -> String {
    print_version();

    let brief = format!("Usage: {} [options]", program);
    opts.usage(&brief)
}

#[cfg(debug_assertions)]
fn setup_logging(verbose: bool) {
    let mut builder = env_logger::Builder::new();
    match env::var("RUST_LOG") {
        Ok(config) => {
            builder.parse_filters(&config);
            builder.init();

            if verbose {
                warn!("`--verbose` flag overidden by `RUST_LOG` environment variable");
            }
        }
        Err(_) => {
            if verbose {
                builder.parse_filters("libmdns=info,librespot=debug,spotty=trace");
            } else {
                builder.parse_filters("libmdns=info,librespot=info,spotty=info");
            }
            builder.init();
        }
    }
}

pub fn get_credentials<F: FnOnce(&String) -> Option<String>>(
    username: Option<String>,
    password: Option<String>,
    cached_credentials: Option<Credentials>,
    prompt: F,
) -> Option<Credentials> {
    if let Some(username) = username {
        if let Some(password) = password {
            return Some(Credentials::with_password(username, password));
        }

        match cached_credentials {
            Some(credentials) if username == credentials.username => Some(credentials),
            _ => {
                let password = prompt(&username)?;
                Some(Credentials::with_password(username, password))
            }
        }
    } else {
        cached_credentials
    }
}

fn print_version() {
    println!(
        "{spottyvers} - using librespot {semver} {sha} (Built on {build_date}, Build ID: {build_id})",
        spottyvers = VERSION,
        semver = version::SEMVER,
        sha = version::SHA_SHORT,
        build_date = version::BUILD_DATE,
        build_id = version::BUILD_ID
    );
}

struct Setup {
    format: AudioFormat,
    backend: SinkBuilder,
    mixer: MixerFn,
    cache: Option<Cache>,
    player_config: PlayerConfig,
    session_config: SessionConfig,
    connect_config: ConnectConfig,
    mixer_config: MixerConfig,
    credentials: Option<Credentials>,
    enable_discovery: bool,
    zeroconf_port: u16,

    // spotty
    authenticate: bool,
    single_track:  Option<String>,
    start_position: u32,
    client_id: Option<String>,
    scopes: Option<String>,
    get_token: bool,
    save_token: Option<String>,
    lms: LMS,
}

fn get_setup(args: &[String]) -> Setup {
    let mut opts = getopts::Options::new();
    opts.optopt(
        "c",
        "cache",
        "Path to a directory where files will be cached.",
        "CACHE",
    ).optflag("", "disable-audio-cache", "(Only here fore compatibility with librespot - audio cache is disabled by default).")
        .optflag("", "enable-audio-cache", "Enable caching of the audio data.")
        .optopt("n", "name", "Device name", "NAME")
        .optopt(
            "b",
            "bitrate",
            "Bitrate (96, 160 or 320). Defaults to 160",
            "BITRATE",
        )
        .optflag("v", "verbose", "Enable verbose output")
        .optflag("V", "version", "Display librespot version string")
        .optopt("u", "username", "Username to sign in with", "USERNAME")
        .optopt("p", "password", "Password", "PASSWORD")
        .optopt("", "proxy", "HTTP proxy to use when connecting", "PROXY")
        .optopt("", "ap-port", "Connect to AP with specified port. If no AP with that port are present fallback AP will be used. Available ports are usually 80, 443 and 4070", "AP_PORT")
        .optflag("", "disable-discovery", "Disable discovery mode")
        .optopt(
            "",
            "initial-volume",
            "Initial volume (%) once connected {0..100}. Defaults to 50 for softvol and for Alsa mixer the current volume.",
            "VOLUME",
        )
        .optopt(
            "",
            "zeroconf-port",
            "The port the internal server advertised over zeroconf uses.",
            "ZEROCONF_PORT",
        )
        .optopt(
            "",
            "dither",
            "Specify the dither algorithm to use - [none, gpdf, tpdf, tpdf_hp]. Defaults to 'tpdf' for formats S16, S24, S24_3 and 'none' for other formats.",
            "DITHER",
        )
        .optflag(
            "",
            "enable-volume-normalisation",
            "Play all tracks at the same volume",
        )
        .optopt(
            "",
            "normalisation-gain-type",
            "Specify the normalisation gain type to use - [track, album]. Default is album.",
            "GAIN_TYPE",
        )
        .optflag(
            "",
            "autoplay",
            "autoplay similar songs when your music ends.",
        )
        .optflag(
            "",
            "disable-gapless",
            "disable gapless playback.",
        )
	    .optflag(
            "",
            "passthrough",
            "Pass raw stream to output, only works for \"pipe\"."
        )
        // spotty
        .optflag("a", "authenticate", "Authenticate given username and password. Make sure you define a cache folder to store credentials.")
        .optopt(
            "",
            "single-track",
            "Play a single track ID and exit.",
            "ID"
        )
        .optopt(
            "",
            "start-position",
            "Position (in seconds) where playback should be started. Only valid with the --single-track option.",
            "STARTPOSITION"
        )
        .optflag(
            "x",
            "check",
            "Run quick internal check"
        )
        .optopt(
            "i",
            "client-id",
            "A Spotify client_id to be used to get the oauth token. Required with the --get-token request.",
            "CLIENT_ID"
        )
        .optopt(
            "",
            "scope",
            "The scopes you want to have access to with the oauth token.",
            "SCOPE"
        )
        .optflag(
            "t",
            "get-token",
            "Get oauth token to be used with the web API etc. and print it to the console."
        )
        .optopt(
            "T",
            "save-token",
            "Get oauth token to be used with the web API etc. and store it in the given file.",
            "TOKENFILE"
        )
        .optflag(
            "",
            "pass-through",
            "Pass raw stream to output, only works for \"pipe\"."
        )
        .optopt(
            "",
            "lms",
            "hostname and port of Logitech Media Server instance (eg. localhost:9000)",
            "LMS"
        )
        .optopt(
            "",
            "lms-auth",
            "Authentication data to access Logitech Media Server",
            "LMSAUTH"
        )
        .optopt(
            "",
            "player-mac",
            "MAC address of the Squeezebox to be controlled",
            "MAC"
        )
        ;

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            eprintln!("error: {}\n{}", f.to_string(), usage(&args[0], &opts));
            exit(1);
        }
    };

    if matches.opt_present("version") {
        print_version();
        exit(0);
    }

    if matches.opt_present("check") {
        spotty::check();
    }


    #[cfg(debug_assertions)]
    {
    let verbose = matches.opt_present("verbose");
    setup_logging(verbose);
    }

    info!(
        "{spottyvers} - using librespot {semver} {sha} (Built on {build_date}, Build ID: {build_id})",
        spottyvers = VERSION,
        semver = version::SEMVER,
        sha = version::SHA_SHORT,
        build_date = version::BUILD_DATE,
        build_id = version::BUILD_ID
    );

    let mixer = mixer::find(Some("softvol")).expect("Invalid mixer");

    let mixer_config = MixerConfig {
        card: String::from("default"),
        control: String::from("PCM"),
        index: 0,
        volume_ctrl: VolumeCtrl::Fixed,
    };

    let cache = {
        let system_dir: Option<String> = matches
            .opt_str("c")
            .map(|p| p.into());

        match Cache::new(system_dir, None, None) {
            Ok(cache) => Some(cache),
            Err(e) => {
                warn!("Cannot create cache: {}", e);
                None
            }
        }
    };

    let initial_volume = matches
        .opt_str("initial-volume")
        .map(|initial_volume| {
            let volume = initial_volume.parse::<u16>().unwrap();
            if volume > 100 {
                error!("Initial volume must be in the range 0-100.");
                // the cast will saturate, not necessary to take further action
            }
            (volume as f32 / 100.0 * VolumeCtrl::MAX_VOLUME as f32) as u16
        })
        .or_else(|| cache.as_ref().and_then(Cache::volume));

    let zeroconf_port = matches
        .opt_str("zeroconf-port")
        .map(|port| port.parse::<u16>().unwrap())
        .unwrap_or(0);

    let name = matches
        .opt_str("name")
        .unwrap_or_else(|| "Librespot".to_string());

    let credentials = {
        let cached_credentials = cache.as_ref().and_then(Cache::credentials);

        let password = |username: &String| -> Option<String> {
            write!(stderr(), "Password for {}: ", username).ok()?;
            stderr().flush().ok()?;
            rpassword::read_password().ok()
        };

        get_credentials(
            matches.opt_str("username"),
            matches.opt_str("password"),
            cached_credentials,
            password,
        )
    };

    let session_config = {
        let device_id = device_id(&name);

        SessionConfig {
            user_agent: version::VERSION_STRING.to_string(),
            device_id,
            proxy: matches.opt_str("proxy").or_else(|| std::env::var("http_proxy").ok()).map(
                |s| {
                    match Url::parse(&s) {
                        Ok(url) => {
                            if url.host().is_none() || url.port_or_known_default().is_none() {
                                panic!("Invalid proxy url, only urls on the format \"http://host:port\" are allowed");
                            }

                            if url.scheme() != "http" {
                                panic!("Only unsecure http:// proxies are supported");
                            }
                            url
                        },
                    Err(err) => panic!("Invalid proxy url: {}, only urls on the format \"http://host:port\" are allowed", err)
                    }
                },
            ),
            ap_port: matches
                .opt_str("ap-port")
                .map(|port| port.parse::<u16>().expect("Invalid port")),
        }
    };

    let passthrough = matches.opt_present("passthrough") || matches.opt_present("pass-through");

    let player_config = {
        let bitrate = matches
            .opt_str("b")
            .as_ref()
            .map(|bitrate| Bitrate::from_str(bitrate).expect("Invalid bitrate"))
            .unwrap_or_default();

        let normalisation_type = matches
            .opt_str("normalisation-gain-type")
            .as_ref()
            .map(|gain_type| {
                NormalisationType::from_str(gain_type).expect("Invalid normalisation type")
            })
            .unwrap_or_default();

        let ditherer = PlayerConfig::default().ditherer;

        PlayerConfig {
            bitrate,
            gapless: !matches.opt_present("disable-gapless"),
            passthrough,
            normalisation: matches.opt_present("enable-volume-normalisation"),
            normalisation_type: normalisation_type,
            normalisation_method: NormalisationMethod::Basic,
            normalisation_pregain: PlayerConfig::default().normalisation_pregain,
            normalisation_threshold: PlayerConfig::default().normalisation_threshold,
            normalisation_attack: PlayerConfig::default().normalisation_attack,
            normalisation_release: PlayerConfig::default().normalisation_release,
            normalisation_knee: PlayerConfig::default().normalisation_knee,
            ditherer,
            lms_connect_mode: !matches.opt_present("single-track"),
        }
    };

    let connect_config = {
        let device_type = DeviceType::default();
        let has_volume_ctrl = !matches!(mixer_config.volume_ctrl, VolumeCtrl::Fixed);
        let autoplay = matches.opt_present("autoplay");

        ConnectConfig {
            name,
            device_type,
            initial_volume,
            has_volume_ctrl,
            autoplay,
        }
    };

    // don't enable discovery while fetching tracks or tokens
    let enable_discovery = !matches.opt_present("disable-discovery")
        && !matches.opt_present("single-track")
        && !matches.opt_present("save-token")
        && !matches.opt_present("get-token");

    let authenticate = matches.opt_present("authenticate");
    let start_position = matches.opt_str("start-position")
        .unwrap_or("0".to_string())
        .parse::<f32>().unwrap_or(0.0);

    let save_token = matches.opt_str("save-token").unwrap_or("".to_string());
    let client_id = matches.opt_str("client-id")
        .unwrap_or(format!("{}", include_str!("client_id.txt")));

    let lms = LMS::new(matches.opt_str("lms"), matches.opt_str("player-mac"), matches.opt_str("lms-auth"));

    Setup {
        format: AudioFormat::default(),
        backend: audio_backend::find(None).unwrap(),
        mixer,
        cache,
        player_config,
        session_config,
        connect_config,
        mixer_config,
        credentials,
        enable_discovery,
        zeroconf_port,
        // spotty
        authenticate,
        single_track: matches.opt_str("single-track"),
        start_position: (start_position * 1000.0) as u32,
        get_token: matches.opt_present("get-token") || save_token.as_str().len() != 0,
        save_token: if save_token.as_str().len() == 0 { None } else { Some(save_token) },
        client_id: if client_id.as_str().len() == 0 { None } else { Some(client_id) },
        scopes: matches.opt_str("scope"),
        lms,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if env::var("RUST_BACKTRACE").is_err() {
        env::set_var("RUST_BACKTRACE", "full")
    }

    let args: Vec<String> = std::env::args().collect();
    let setup = get_setup(&args);

    let mut last_credentials = None;
    let mut spirc: Option<Spirc> = None;
    let mut spirc_task: Option<Pin<_>> = None;
    let mut player_event_channel: Option<UnboundedReceiver<PlayerEvent>> = None;
    let mut auto_connect_times: Vec<Instant> = vec![];
    let mut discovery = None;
    let mut connecting: Pin<Box<dyn future::FusedFuture<Output = _>>> = Box::pin(future::pending());

    if setup.enable_discovery {
        let device_id = setup.session_config.device_id.clone();

        discovery = Some(
            librespot::discovery::Discovery::builder(device_id)
                .name(setup.connect_config.name.clone())
                .device_type(setup.connect_config.device_type)
                .port(setup.zeroconf_port)
                .launch()
                .unwrap(),
        );
    }

    if let Some(credentials) = setup.credentials {
        last_credentials = Some(credentials.clone());
        connecting = Box::pin(
            Session::connect(
                setup.session_config.clone(),
                credentials,
                setup.cache.clone(),
            )
            .fuse(),
        );
    }

    if let Some(ref track_id) = setup.single_track {
        spotty::play_track(track_id.to_string(), setup.start_position, last_credentials, setup.player_config, setup.session_config).await;
        exit(0);
    }
    else if setup.get_token {
        spotty::get_token(setup.client_id, setup.scopes, setup.save_token, last_credentials, setup.session_config).await;
        exit(0);
    }

    loop {
        tokio::select! {
            credentials = async { discovery.as_mut().unwrap().next().await }, if discovery.is_some() => {
                match credentials {
                    Some(credentials) => {
                        last_credentials = Some(credentials.clone());
                        auto_connect_times.clear();

                        if let Some(spirc) = spirc.take() {
                            spirc.shutdown();
                        }
                        if let Some(spirc_task) = spirc_task.take() {
                            // Continue shutdown in its own task
                            tokio::spawn(spirc_task);
                        }

                        connecting = Box::pin(Session::connect(
                            setup.session_config.clone(),
                            credentials,
                            setup.cache.clone(),
                        ).fuse());
                    },
                    None => {
                        warn!("Discovery stopped!");
                        discovery = None;
                    }
                }
            },
            session = &mut connecting, if !connecting.is_terminated() => match session {
                Ok(session) => {
                    // Spotty auth mode: exit after saving credentials
                    if setup.authenticate {
                        break;
                    }

                    let mixer_config = setup.mixer_config.clone();
                    let mixer = (setup.mixer)(mixer_config);
                    let player_config = setup.player_config.clone();
                    let connect_config = setup.connect_config.clone();

                    let audio_filter = mixer.get_audio_filter();
                    let format = setup.format;
                    let backend = setup.backend;
                    let device = Some(NULLDEVICE.to_string());
                    let (player, event_channel) =
                        Player::new(player_config, session.clone(), audio_filter, move || {
                            (backend)(device, format)
                        });

                    let (spirc_, spirc_task_) = Spirc::new(connect_config, session, player, mixer);

                    spirc = Some(spirc_);
                    spirc_task = Some(Box::pin(spirc_task_));
                    player_event_channel = Some(event_channel);
                },
                Err(e) => {
                    warn!("Connection failed: {}", e);
                }
            },
            _ = async { spirc_task.as_mut().unwrap().await }, if spirc_task.is_some() => {
                spirc_task = None;

                warn!("Spirc shut down unexpectedly");
                while !auto_connect_times.is_empty()
                    && ((Instant::now() - auto_connect_times[0]).as_secs() > 600)
                {
                    let _ = auto_connect_times.remove(0);
                }

                if let Some(credentials) = last_credentials.clone() {
                    if auto_connect_times.len() >= 5 {
                        warn!("Spirc shut down too often. Not reconnecting automatically.");
                    } else {
                        auto_connect_times.push(Instant::now());

                        connecting = Box::pin(Session::connect(
                            setup.session_config.clone(),
                            credentials,
                            setup.cache.clone(),
                        ).fuse());
                    }
                }
            },
            event = async { player_event_channel.as_mut().unwrap().recv().await }, if player_event_channel.is_some() => match event {
                Some(event) => {
                    setup.lms.signal_event(event).await;
                },
                None => {
                    player_event_channel = None;
                }
            },
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    info!("Gracefully shutting down");

    // Shutdown spirc if necessary
    if let Some(spirc) = spirc {
        spirc.shutdown();

        if let Some(mut spirc_task) = spirc_task {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => (),
                _ = spirc_task.as_mut() => ()
            }
        }
    }
}
