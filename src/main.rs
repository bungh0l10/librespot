#[macro_use] extern crate serde_json;

use futures_util::{future, FutureExt, StreamExt};
use librespot_playback::player::PlayerEvent;
use log::{error, info, trace, warn};
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
use librespot::playback::mixer::softmixer::SoftMixer;
use librespot::playback::player::Player;

mod spotty;
use spotty::{LMS};

use std::env;
use std::io::{stderr, Write};
use std::path::Path;
use std::pin::Pin;
use std::process::exit;
use std::str::FromStr;
use std::time::Instant;

const VERSION: &'static str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));

#[cfg(target_os="windows")]
const NULLDEVICE: &'static str = "NUL";
#[cfg(not(target_os="windows"))]
const NULLDEVICE: &'static str = "/dev/null";

fn device_id(name: &str) -> String {
    hex::encode(Sha1::digest(name.as_bytes()))
}

fn usage(program: &str, opts: &getopts::Options) -> String {
    println!("{}", get_version_string());


    let brief = format!("Usage: {} [options]", program);
    opts.usage(&brief)
}

fn arg_to_var(arg: &str) -> String {
    // To avoid name collisions environment variables must be prepended
    // with `LIBRESPOT_` so option/flag `foo-bar` becomes `LIBRESPOT_FOO_BAR`.
    format!("LIBRESPOT_{}", arg.to_uppercase().replace("-", "_"))
}

fn env_var_present(arg: &str) -> bool {
    env::var(arg_to_var(arg)).is_ok()
}

fn env_var_opt_str(option: &str) -> Option<String> {
    match env::var(arg_to_var(option)) {
        Ok(value) => Some(value),
        Err(_) => None,
    }
}

#[cfg(debug_assertions)]
fn setup_logging(quiet: bool, verbose: bool) {
    let mut builder = env_logger::Builder::new();
    match env::var("RUST_LOG") {
        Ok(config) => {
            builder.parse_filters(&config);
            builder.init();

            if verbose {
                warn!("`--verbose` flag overidden by `RUST_LOG` environment variable");
            } else if quiet {
                warn!("`--quiet` flag overidden by `RUST_LOG` environment variable");
            }
        }
        Err(_) => {
            if verbose {
                builder.parse_filters("libmdns=info,librespot=trace,spotty=trace");
            } else if quiet {
                builder.parse_filters("libmdns=warn,librespot=warn,spotty=warn");
            } else {
                builder.parse_filters("libmdns=info,librespot=info,spotty=info");
            }
            builder.init();

            if verbose && quiet {
                warn!("`--verbose` and `--quiet` are mutually exclusive. Logging can not be both verbose and quiet. Using verbose mode.");
            }
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

fn get_version_string() -> String {
    #[cfg(debug_assertions)]
    const BUILD_PROFILE: &str = "debug";
    #[cfg(not(debug_assertions))]
    const BUILD_PROFILE: &str = "release";

    format!(
        "{spottyvers} - using librespot {semver} {sha} (Built on {build_date}, Build ID: {build_id}, Profile: {build_profile})",
        spottyvers = VERSION,
        semver = version::SEMVER,
        sha = version::SHA_SHORT,
        build_date = version::BUILD_DATE,
        build_id = version::BUILD_ID,
        build_profile = BUILD_PROFILE
    )
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
    const AP_PORT: &str = "ap-port";
    const AUTHENTICATE: &str = "authenticate";
    const AUTOPLAY: &str = "autoplay";
    const BITRATE: &str = "bitrate";
    const CACHE: &str = "cache";
    const CHECK: &str = "check";
    const CLIENT_ID: &str = "client-id";
    const DISABLE_AUDIO_CACHE: &str = "disable-audio-cache";
    const DISABLE_DISCOVERY: &str = "disable-discovery";
    const DISABLE_GAPLESS: &str = "disable-gapless";
    const ENABLE_AUDIO_CACHE: &str = "enable-audio-cache";
    const ENABLE_VOLUME_NORMALISATION: &str = "enable-volume-normalisation";
    const GET_TOKEN: &str = "get-token";
    const HELP: &str = "help";
    const INITIAL_VOLUME: &str = "initial-volume";
    const LMS_AUTH: &str = "lms-auth";
    const LOGITECH_MEDIA_SERVER: &str = "lms";
    const NAME: &str = "name";
    const NORMALISATION_GAIN_TYPE: &str = "normalisation-gain-type";
    const PASSTHROUGH: &str = "passthrough";
    const PASS_THROUGH: &str = "pass-through";
    const PASSWORD: &str = "password";
    const PLAYER_MAC: &str = "player-mac";
    const PROXY: &str = "proxy";
    const SAVE_TOKEN: &str = "save-token";
    const SCOPE: &str = "scope";
    const SINGLE_TRACK: &str = "single-track";
    const START_POSITION: &str = "start-position";
    const QUIET: &str = "quiet";
    const USERNAME: &str = "username";
    const VERBOSE: &str = "verbose";
    const VERSION: &str = "version";
    const ZEROCONF_PORT: &str = "zeroconf-port";

    // Mostly arbitrary.
    const AUTHENTICATE_SHORT: &str="a";
    const AUTOPLAY_SHORT: &str = "A";
    const AP_PORT_SHORT: &str = "";
    const BITRATE_SHORT: &str = "b";
    const CACHE_SHORT: &str = "c";
    const DISABLE_AUDIO_CACHE_SHORT: &str = "G";
    const ENABLE_AUDIO_CACHE_SHORT: &str = "";
    const DISABLE_GAPLESS_SHORT: &str = "g";
    const HELP_SHORT: &str = "h";
    const CLIENT_ID_SHORT: &str = "i";
    const ENABLE_VOLUME_NORMALISATION_SHORT: &str = "N";
    const NAME_SHORT: &str = "n";
    const DISABLE_DISCOVERY_SHORT: &str = "O";
    const PASSTHROUGH_SHORT: &str = "P";
    const PASSWORD_SHORT: &str = "p";
    const QUIET_SHORT: &str = "q";
    const INITIAL_VOLUME_SHORT: &str = "R";
    const GET_TOKEN_SHORT: &str = "t";
    const SAVE_TOKEN_SHORT: &str = "T";
    const USERNAME_SHORT: &str = "u";
    const VERSION_SHORT: &str = "V";
    const VERBOSE_SHORT: &str = "v";
    const NORMALISATION_GAIN_TYPE_SHORT: &str = "W";
    const CHECK_SHORT: &str = "x";
    const PROXY_SHORT: &str = "";
    const ZEROCONF_PORT_SHORT: &str = "z";

    // Options that have different desc's
    // depending on what backends were enabled at build time.
    const INITIAL_VOLUME_DESC: &str = "Initial volume in % from 0 - 100. Defaults to 50.";

    let mut opts = getopts::Options::new();
    opts.optflag(
        HELP_SHORT,
        HELP,
        "Print this help menu.",
    )
    .optflag(
        VERSION_SHORT,
        VERSION,
        "Display librespot version string.",
    )
    .optflag(
        VERBOSE_SHORT,
        VERBOSE,
        "Enable verbose log output.",
    )
    .optflag(
        QUIET_SHORT,
        QUIET,
        "Only log warning and error messages.",
    )
    .optflag(
        DISABLE_AUDIO_CACHE_SHORT,
        DISABLE_AUDIO_CACHE,
        "(Only here fore compatibility with librespot - audio cache is disabled by default).",
    )
    .optflag(
        ENABLE_AUDIO_CACHE_SHORT,
        ENABLE_AUDIO_CACHE,
        "Enable caching of the audio data."
    )
    .optflag(
        DISABLE_DISCOVERY_SHORT,
        DISABLE_DISCOVERY,
        "Disable zeroconf discovery mode.",
    )
    .optflag(
        DISABLE_GAPLESS_SHORT,
        DISABLE_GAPLESS,
        "Disable gapless playback.",
    )
    .optflag(
        AUTOPLAY_SHORT,
        AUTOPLAY,
        "Automatically play similar songs when your music ends.",
    )
    .optflag(
        PASSTHROUGH_SHORT,
        PASSTHROUGH,
        "Pass a raw stream to the output. Only works with the pipe and subprocess backends.",
    )
    .optflag(
        ENABLE_VOLUME_NORMALISATION_SHORT,
        ENABLE_VOLUME_NORMALISATION,
        "Play all tracks at approximately the same apparent volume.",
    )
    .optopt(
        NAME_SHORT,
        NAME,
        "Device name. Defaults to Spotty.",
        "NAME",
    )
    .optopt(
        BITRATE_SHORT,
        BITRATE,
        "Bitrate (kbps) {96|160|320}. Defaults to 160.",
        "BITRATE",
    )
    .optopt(
        CACHE_SHORT,
        CACHE,
        "Path to a directory where files will be cached.",
        "PATH",
    )
    .optopt(
        USERNAME_SHORT,
        USERNAME,
        "Username used to sign in with.",
        "USERNAME",
    )
    .optopt(
        PASSWORD_SHORT,
        PASSWORD,
        "Password used to sign in with.",
        "PASSWORD",
    )
    .optopt(
        INITIAL_VOLUME_SHORT,
        INITIAL_VOLUME,
        INITIAL_VOLUME_DESC,
        "VOLUME",
    )
    .optopt(
        NORMALISATION_GAIN_TYPE_SHORT,
        NORMALISATION_GAIN_TYPE,
        "Specify the normalisation gain type to use {track|album|auto}. Defaults to auto.",
        "TYPE",
    )
    .optopt(
        ZEROCONF_PORT_SHORT,
        ZEROCONF_PORT,
        "The port the internal server advertises over zeroconf 1 - 65535. Ports <= 1024 may require root privileges.",
        "PORT",
    )
    .optopt(
        PROXY_SHORT,
        PROXY,
        "HTTP proxy to use when connecting.",
        "URL",
    )
    .optopt(
        AP_PORT_SHORT,
        AP_PORT,
        "Connect to an AP with a specified port 1 - 65535. If no AP with that port is present a fallback AP will be used. Available ports are usually 80, 443 and 4070.",
        "PORT",
    )
    // spotty
    .optflag(
        AUTHENTICATE_SHORT,
        AUTHENTICATE,
        "Authenticate given username and password. Make sure you define a cache folder to store credentials."
    )
    .optopt(
        "",
        SINGLE_TRACK,
        "Play a single track ID and exit.",
        "ID"
    )
    .optopt(
        "",
        START_POSITION,
        "Position (in seconds) where playback should be started. Only valid with the --single-track option.",
        "STARTPOSITION"
    )
    .optflag(
        CHECK_SHORT,
        CHECK,
        "Run quick internal check"
    )
    .optopt(
        CLIENT_ID_SHORT,
        CLIENT_ID,
        "A Spotify client_id to be used to get the oauth token. Required with the --get-token request.",
        "CLIENT_ID"
    )
    .optopt(
        "",
        SCOPE,
        "The scopes you want to have access to with the oauth token.",
        "SCOPE"
    )
    .optflag(
        GET_TOKEN_SHORT,
        GET_TOKEN,
        "Get oauth token to be used with the web API etc. and print it to the console."
    )
    .optopt(
        SAVE_TOKEN_SHORT,
        SAVE_TOKEN,
        "Get oauth token to be used with the web API etc. and store it in the given file.",
        "TOKENFILE"
    )
    .optflag(
        "",
        PASS_THROUGH,
        "Pass raw stream to output, only works for \"pipe\"."
    )
    .optopt(
        "",
        LOGITECH_MEDIA_SERVER,
        "hostname and port of Logitech Media Server instance (eg. localhost:9000)",
        "LMS"
    )
    .optopt(
        "",
        LMS_AUTH,
        "Authentication data to access Logitech Media Server",
        "LMSAUTH"
    )
    .optopt(
        "",
        PLAYER_MAC,
        "MAC address of the Squeezebox to be controlled",
        "MAC"
    );

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            eprintln!(
                "Error parsing command line options: {}\n{}",
                f,
                usage(&args[0], &opts)
            );
            exit(1);
        }
    };

    let opt_present = |opt| matches.opt_present(opt) || env_var_present(opt);

    let opt_str = |opt| {
        if matches.opt_present(opt) {
            matches.opt_str(opt)
        } else {
            env_var_opt_str(opt)
        }
    };

    if opt_present(HELP) {
        println!("{}", usage(&args[0], &opts));
        exit(0);
    }

    if opt_present(VERSION) {
        println!("{}", get_version_string());
        exit(0);
    }

    if opt_present(CHECK) {
        spotty::check(get_version_string());
    }

    #[cfg(debug_assertions)]
    setup_logging(opt_present(QUIET), opt_present(VERBOSE));

    info!("{}", get_version_string());

    let librespot_env_vars: Vec<String> = env::vars_os()
        .filter_map(|(k, v)| {
            let mut env_var = None;
            if let Some(key) = k.to_str() {
                if key.starts_with("LIBRESPOT_") {
                    if matches!(key, "LIBRESPOT_PASSWORD" | "LIBRESPOT_USERNAME") {
                        // Don't log creds.
                        env_var = Some(format!("\t\t{}=XXXXXXXX", key));
                    } else if let Some(value) = v.to_str() {
                        env_var = Some(format!("\t\t{}={}", key, value));
                    }
                }
            }

            env_var
        })
        .collect();

    if !librespot_env_vars.is_empty() {
        trace!("Environment variable(s):");

        for kv in librespot_env_vars {
            trace!("{}", kv);
        }
    }

    let cmd_args = &args[1..];

    let cmd_args_len = cmd_args.len();

    if cmd_args_len > 0 {
        trace!("Command line argument(s):");

        for (index, key) in cmd_args.iter().enumerate() {
            if key.starts_with('-') || key.starts_with("--") {
                if matches!(key.as_str(), "--password" | "-p" | "--username" | "-u") {
                    // Don't log creds.
                    trace!("\t\t{} XXXXXXXX", key);
                } else {
                    let mut value = "".to_string();
                    let next = index + 1;
                    if next < cmd_args_len {
                        let next_key = cmd_args[next].clone();
                        if !next_key.starts_with('-') && !next_key.starts_with("--") {
                            value = next_key;
                        }
                    }

                    trace!("\t\t{} {}", key, value);
                }
            }
        }
    }

    let mixer = mixer::find(Some(SoftMixer::NAME).as_deref()).expect("Invalid mixer");
    let mixer_type: Option<String> = None;

    let mixer_config = {
        let mixer_default_config = MixerConfig::default();

        let device = mixer_default_config.device;

        let index = mixer_default_config.index;

        let control = mixer_default_config.control;

        let volume_ctrl = VolumeCtrl::Linear;

        MixerConfig {
            device,
            control,
            index,
            volume_ctrl,
        }
    };

    let cache = {
        let volume_dir = opt_str(CACHE)
            .map(|p| p.into());

        let cred_dir = volume_dir.clone();

        let audio_dir = if opt_present(DISABLE_AUDIO_CACHE) {
            None
        } else {
            opt_str(CACHE)
                .as_ref()
                .map(|p| AsRef::<Path>::as_ref(p).join("files"))
        };

        let limit = None;

        match Cache::new(cred_dir, volume_dir, audio_dir, limit) {
            Ok(cache) => Some(cache),
            Err(e) => {
                warn!("Cannot create cache: {}", e);
                None
            }
        }
    };

    let credentials = {
        let cached_credentials = cache.as_ref().and_then(Cache::credentials);

        let password = |username: &String| -> Option<String> {
            write!(stderr(), "Password for {}: ", username).ok()?;
            stderr().flush().ok()?;
            rpassword::read_password().ok()
        };

        get_credentials(
            opt_str(USERNAME),
            opt_str(PASSWORD),
            cached_credentials,
            password,
        )
    };

    // don't enable discovery while fetching tracks or tokens
    let enable_discovery = !opt_present(DISABLE_DISCOVERY)
        && !opt_present(SINGLE_TRACK)
        && !opt_present(SAVE_TOKEN)
        && !opt_present(GET_TOKEN);

    if credentials.is_none() && !enable_discovery {
        error!("Credentials are required if discovery is disabled.");
        exit(1);
    }

    if !enable_discovery && opt_present(ZEROCONF_PORT) {
        warn!(
            "With the `--{}` / `-{}` flag set `--{}` / `-{}` has no effect.",
            DISABLE_DISCOVERY, DISABLE_DISCOVERY_SHORT, ZEROCONF_PORT, ZEROCONF_PORT_SHORT
        );
    }

    let zeroconf_port = if enable_discovery {
        opt_str(ZEROCONF_PORT)
            .map(|port| {
                let on_error = || {
                    error!(
                        "Invalid `--{}` / `-{}`: {}",
                        ZEROCONF_PORT, ZEROCONF_PORT_SHORT, port
                    );
                    println!(
                        "Valid `--{}` / `-{}` values: 1 - 65535",
                        ZEROCONF_PORT, ZEROCONF_PORT_SHORT
                    );
                };

                let port = port.parse::<u16>().unwrap_or_else(|_| {
                    on_error();
                    exit(1);
                });

                if port == 0 {
                    on_error();
                    exit(1);
                }

                port
            })
            .unwrap_or(0)
    } else {
        0
    };

    let connect_config = {
        let connect_default_config = ConnectConfig::default();

        let name = opt_str(NAME).unwrap_or_else(|| connect_default_config.name.clone());

        let initial_volume = opt_str(INITIAL_VOLUME)
            .map(|initial_volume| {
                let on_error = || {
                    error!(
                        "Invalid `--{}` / `-{}`: {}",
                        INITIAL_VOLUME, INITIAL_VOLUME_SHORT, initial_volume
                    );
                    println!(
                        "Valid `--{}` / `-{}` values: 0 - 100",
                        INITIAL_VOLUME, INITIAL_VOLUME_SHORT
                    );
                    println!(
                        "Default: {}",
                        connect_default_config.initial_volume.unwrap_or_default()
                    );
                };

                let volume = initial_volume.parse::<u16>().unwrap_or_else(|_| {
                    on_error();
                    exit(1);
                });

                if volume > 100 {
                    on_error();
                    exit(1);
                }

                (volume as f32 / 100.0 * VolumeCtrl::MAX_VOLUME as f32) as u16
            })
            .or_else(|| match mixer_type.as_deref() {
                _ => cache.as_ref().and_then(Cache::volume),
            });

        let device_type = DeviceType::default();
        let has_volume_ctrl = !matches!(mixer_config.volume_ctrl, VolumeCtrl::Fixed);
        let autoplay = opt_present(AUTOPLAY);

        ConnectConfig {
            name,
            device_type,
            initial_volume,
            has_volume_ctrl,
            autoplay,
        }
    };

    let session_config = {
        let device_id = device_id(&connect_config.name);

        SessionConfig {
            user_agent: version::VERSION_STRING.to_string(),
            device_id,
            proxy: opt_str(PROXY).or_else(|| std::env::var("http_proxy").ok()).map(
                |s| {
                    match Url::parse(&s) {
                        Ok(url) => {
                            if url.host().is_none() || url.port_or_known_default().is_none() {
                                error!("Invalid proxy url, only URLs on the format \"http://host:port\" are allowed");
                                exit(1);
                            }

                            if url.scheme() != "http" {
                                error!("Only unsecure http:// proxies are supported");
                                exit(1);
                            }

                            url
                        },
                        Err(e) => {
                            error!("Invalid proxy URL: {}, only URLs in the format \"http://host:port\" are allowed", e);
                            exit(1);
                        }
                    }
                },
            ),
            ap_port: opt_str(AP_PORT)
                .map(|port| {
                    let on_error = || {
                        error!("Invalid `--{}` / `-{}`: {}", AP_PORT, AP_PORT_SHORT, port);
                        println!("Valid `--{}` / `-{}` values: 1 - 65535", AP_PORT, AP_PORT_SHORT);
                    };

                    let port = port.parse::<u16>().unwrap_or_else(|_| {
                        on_error();
                        exit(1);
                    });

                    if port == 0 {
                        on_error();
                        exit(1);
                    }

                    port
                }),
        }
    };

    let passthrough = opt_present(PASSTHROUGH) || opt_present(PASS_THROUGH);

    let player_config = {
        let player_default_config = PlayerConfig::default();

        let bitrate = opt_str(BITRATE)
            .as_deref()
            .map(|bitrate| {
                Bitrate::from_str(bitrate).unwrap_or_else(|_| {
                    error!(
                        "Invalid `--{}` / `-{}`: {}",
                        BITRATE, BITRATE_SHORT, bitrate
                    );
                    println!(
                        "Valid `--{}` / `-{}` values: 96, 160, 320",
                        BITRATE, BITRATE_SHORT
                    );
                    println!("Default: 160");
                    exit(1);
                })
            })
            .unwrap_or(player_default_config.bitrate);

        let gapless = !opt_present(DISABLE_GAPLESS);

        let normalisation = opt_present(ENABLE_VOLUME_NORMALISATION);

        let normalisation_type;

        if !normalisation {
            for a in &[
                NORMALISATION_GAIN_TYPE,
            ] {
                if opt_present(a) {
                    warn!(
                        "Without the `--{}` / `-{}` flag normalisation options have no effect.",
                        ENABLE_VOLUME_NORMALISATION, ENABLE_VOLUME_NORMALISATION_SHORT,
                    );
                    break;
                }
            }

            normalisation_type = player_default_config.normalisation_type;
        } else {
            normalisation_type = opt_str(NORMALISATION_GAIN_TYPE)
                .as_deref()
                .map(|gain_type| {
                    NormalisationType::from_str(gain_type).unwrap_or_else(|_| {
                        error!(
                            "Invalid `--{}` / `-{}`: {}",
                            NORMALISATION_GAIN_TYPE, NORMALISATION_GAIN_TYPE_SHORT, gain_type
                        );
                        println!(
                            "Valid `--{}` / `-{}` values: track, album, auto",
                            NORMALISATION_GAIN_TYPE, NORMALISATION_GAIN_TYPE_SHORT,
                        );
                        println!("Default: {:?}", player_default_config.normalisation_type);
                        exit(1);
                    })
                })
                .unwrap_or(player_default_config.normalisation_type);
        }

        let ditherer = PlayerConfig::default().ditherer;

        PlayerConfig {
            bitrate,
            gapless,
            passthrough,
            normalisation,
            normalisation_type,
            normalisation_method: NormalisationMethod::Basic,
            normalisation_pregain: PlayerConfig::default().normalisation_pregain,
            normalisation_threshold: PlayerConfig::default().normalisation_threshold,
            normalisation_attack: PlayerConfig::default().normalisation_attack,
            normalisation_release: PlayerConfig::default().normalisation_release,
            normalisation_knee: PlayerConfig::default().normalisation_knee,
            ditherer,
            lms_connect_mode: !opt_present(SINGLE_TRACK),
        }
    };

    let authenticate = opt_present(AUTHENTICATE);
    let start_position = opt_str(START_POSITION)
        .unwrap_or("0".to_string())
        .parse::<f32>().unwrap_or(0.0);

    let save_token = opt_str(SAVE_TOKEN).unwrap_or("".to_string());
    let client_id = opt_str(CLIENT_ID)
        .unwrap_or(format!("{}", include_str!("client_id.txt")));

    let lms = LMS::new(opt_str(LOGITECH_MEDIA_SERVER), opt_str(PLAYER_MAC), opt_str(LMS_AUTH));

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
        single_track: opt_str(SINGLE_TRACK),
        start_position: (start_position * 1000.0) as u32,
        get_token: opt_present(GET_TOKEN) || save_token.as_str().len() != 0,
        save_token: if save_token.as_str().len() == 0 { None } else { Some(save_token) },
        client_id: if client_id.as_str().len() == 0 { None } else { Some(client_id) },
        scopes: opt_str(SCOPE),
        lms,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    const RUST_BACKTRACE: &str = "RUST_BACKTRACE";
    if env::var(RUST_BACKTRACE).is_err() {
        env::set_var(RUST_BACKTRACE, "full")
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
                    error!("Connection failed: {}", e);
                    exit(1);
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
