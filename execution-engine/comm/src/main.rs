extern crate clap;
extern crate common;
extern crate dirs;
extern crate execution_engine;
extern crate grpc;
#[macro_use]
extern crate lazy_static;
extern crate lmdb;
extern crate protobuf;
extern crate shared;
extern crate storage;
extern crate wabt;
extern crate wasm_prep;

pub mod engine_server;

use std::collections::btree_map::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic;
use std::sync::Arc;

use clap::{App, Arg, ArgMatches};
use dirs::home_dir;
use engine_server::*;
use execution_engine::engine::EngineState;
use lmdb::DatabaseFlags;

use shared::logging::log_level::LogLevel;
use shared::logging::log_settings::{LogLevelFilter, LogSettings};
use shared::logging::{log_level, log_settings};
use shared::{logging, socket};
use storage::global_state::lmdb::LmdbGlobalState;
use storage::history::trie_store::lmdb::{LmdbEnvironment, LmdbTrieStore};

// exe / proc
const PROC_NAME: &str = "casperlabs-engine-grpc-server";
const APP_NAME: &str = "Execution Engine Server";
const SERVER_START_MESSAGE: &str = "starting Execution Engine Server";
const SERVER_LISTENING_TEMPLATE: &str = "{listener} is listening on socket: {socket}";
const SERVER_START_EXPECT: &str = "failed to start Execution Engine Server";
#[allow(dead_code)]
const SERVER_STOP_MESSAGE: &str = "stopping Execution Engine Server";

// data-dir / lmdb
const ARG_DATA_DIR: &str = "data-dir";
const ARG_DATA_DIR_SHORT: &str = "d";
const ARG_DATA_DIR_VALUE: &str = "DIR";
const ARG_DATA_DIR_HELP: &str = "Sets the data directory";
const DEFAULT_DATA_DIR_RELATIVE: &str = ".casperlabs";
const GLOBAL_STATE_DIR: &str = "global_state";
const GET_HOME_DIR_EXPECT: &str = "Could not get home directory";
const CREATE_DATA_DIR_EXPECT: &str = "Could not create directory";
const LMDB_ENVIRONMENT_EXPECT: &str = "Could not create LmdbEnvironment";
const LMDB_TRIE_STORE_EXPECT: &str = "Could not create LmdbTrieStore";
const LMDB_GLOBAL_STATE_EXPECT: &str = "Could not create LmdbGlobalState";

// socket
const ARG_SOCKET: &str = "socket";
const ARG_SOCKET_HELP: &str = "socket file";
const ARG_SOCKET_EXPECT: &str = "socket required";
const REMOVING_SOCKET_FILE_MESSAGE: &str = "removing old socket file";
const REMOVING_SOCKET_FILE_EXPECT: &str = "failed to remove old socket file";

// loglevel
const ARG_LOG_LEVEL: &str = "loglevel";
const ARG_LOG_LEVEL_VALUE: &str = "LOGLEVEL";
const ARG_LOG_LEVEL_HELP: &str = "[ fatal | error | warning | info | debug ]";

// if true expect command line args if false use default values
static CHECK_ARGS: atomic::AtomicBool = atomic::AtomicBool::new(false);

// command line args
lazy_static! {
    static ref ARG_MATCHES: clap::ArgMatches<'static> = get_args();
}

// single log_settings instance for app
lazy_static! {
    static ref LOG_SETTINGS: log_settings::LogSettings = get_log_settings();
}

fn main() {
    CHECK_ARGS.store(true, atomic::Ordering::SeqCst);

    set_panic_hook();

    log_server_info(SERVER_START_MESSAGE);

    let matches: &clap::ArgMatches = &*ARG_MATCHES;

    let socket = get_socket(matches);

    if socket.file_exists() {
        log_server_info(REMOVING_SOCKET_FILE_MESSAGE);
        socket.remove_file().expect(REMOVING_SOCKET_FILE_EXPECT);
    }

    let data_dir = get_data_dir(matches);

    let _server = get_grpc_server(&socket, data_dir);

    log_listening_message(&socket);

    // loop indefinitely
    loop {
        std::thread::park();
    }

    // currently unreachable
    // TODO: recommend we impl signal capture; SIGINT at the very least.
    // seems like there are multiple valid / accepted rusty approaches available
    // https://rust-lang-nursery.github.io/cli-wg/in-depth/signals.html

    //log_server_info(SERVER_STOP_MESSAGE);
}

/// capture and log panic information.
fn set_panic_hook() {
    let log_settings_panic = LOG_SETTINGS.clone();
    let hook: Box<dyn Fn(&std::panic::PanicInfo) + 'static + Sync + Send> =
        Box::new(move |panic_info| {
            if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
                let panic_message = format!("{:?}", s);
                logging::log(
                    &log_settings_panic,
                    log_level::LogLevel::Fatal,
                    &panic_message,
                );
            }
            log_server_info(SERVER_STOP_MESSAGE);
        });
    std::panic::set_hook(hook);
}

// get command line arguments
fn get_args() -> ArgMatches<'static> {
    App::new(APP_NAME)
        .arg(
            Arg::with_name(ARG_LOG_LEVEL)
                .required(true)
                .long(ARG_LOG_LEVEL)
                .takes_value(true)
                .value_name(ARG_LOG_LEVEL_VALUE)
                .help(ARG_LOG_LEVEL_HELP),
        )
        .arg(
            Arg::with_name(ARG_DATA_DIR)
                .short(ARG_DATA_DIR_SHORT)
                .long(ARG_DATA_DIR)
                .value_name(ARG_DATA_DIR_VALUE)
                .help(ARG_DATA_DIR_HELP)
                .takes_value(true),
        )
        .arg(
            Arg::with_name(ARG_SOCKET)
                .required(true)
                .help(ARG_SOCKET_HELP)
                .index(1),
        )
        .get_matches()
}

// get value of socket argument
fn get_socket(matches: &ArgMatches) -> socket::Socket {
    let socket = matches.value_of(ARG_SOCKET).expect(ARG_SOCKET_EXPECT);

    socket::Socket::new(socket.to_owned())
}

// get value of data-dir argument
fn get_data_dir(matches: &ArgMatches) -> PathBuf {
    let mut buf = matches.value_of(ARG_DATA_DIR).map_or(
        {
            let mut dir = home_dir().expect(GET_HOME_DIR_EXPECT);
            dir.push(DEFAULT_DATA_DIR_RELATIVE);
            dir
        },
        PathBuf::from,
    );
    buf.push(GLOBAL_STATE_DIR);
    fs::create_dir_all(&buf).unwrap_or_else(|_| panic!("{}: {:?}", CREATE_DATA_DIR_EXPECT, buf));
    buf
}

// build and return a grpc server
fn get_grpc_server(socket: &socket::Socket, data_dir: PathBuf) -> grpc::Server {
    let engine_state = get_engine_state(data_dir);

    engine_server::new(socket.as_str(), engine_state)
        .build()
        .expect(SERVER_START_EXPECT)
}

// init and return engine global state
fn get_engine_state(data_dir: PathBuf) -> EngineState<LmdbGlobalState> {
    let environment = {
        let ret = LmdbEnvironment::new(&data_dir).expect(LMDB_ENVIRONMENT_EXPECT);
        Arc::new(ret)
    };

    let trie_store = {
        let ret = LmdbTrieStore::new(&environment, None, DatabaseFlags::empty())
            .expect(LMDB_TRIE_STORE_EXPECT);
        Arc::new(ret)
    };

    let global_state = {
        let init_state = storage::global_state::mocked_account([48u8; 20]);
        LmdbGlobalState::from_pairs(
            Arc::clone(&environment),
            Arc::clone(&trie_store),
            &init_state,
        )
        .expect(LMDB_GLOBAL_STATE_EXPECT)
    };

    EngineState::new(global_state)
}

// init and return log_settings
fn get_log_settings() -> log_settings::LogSettings {
    if CHECK_ARGS.load(atomic::Ordering::SeqCst) {
        let matches: &clap::ArgMatches = &*ARG_MATCHES;

        let log_level_filter = get_log_level_filter(matches.value_of(ARG_LOG_LEVEL));

        return LogSettings::new(PROC_NAME, log_level_filter);
    }

    LogSettings::new(
        PROC_NAME,
        log_settings::LogLevelFilter::new(LogLevel::Debug),
    )
}

// get value of loglevel argument
fn get_log_level_filter(input: Option<&str>) -> LogLevelFilter {
    let log_level = match input {
        Some(input) => match input {
            "fatal" => LogLevel::Fatal,
            "error" => LogLevel::Error,
            "warning" => LogLevel::Warning,
            "debug" => LogLevel::Debug,
            _ => LogLevel::Info,
        },
        None => log_level::LogLevel::Info,
    };

    log_settings::LogLevelFilter::new(log_level)
}

// log listening on socket message
fn log_listening_message(socket: &socket::Socket) {
    let mut properties: BTreeMap<String, String> = BTreeMap::new();

    properties.insert("listener".to_string(), PROC_NAME.to_owned());
    properties.insert("socket".to_string(), socket.value());

    logging::log_props(
        &*LOG_SETTINGS,
        log_level::LogLevel::Info,
        (&*SERVER_LISTENING_TEMPLATE).to_string(),
        properties,
    );
}

// log server status info messages
fn log_server_info(message: &str) {
    logging::log(&*LOG_SETTINGS, log_level::LogLevel::Info, message);
}
