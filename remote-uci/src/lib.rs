mod engine;
pub mod uci;
mod ws;

use std::{
    net::{SocketAddr, TcpListener},
    ops::Not,
    path::PathBuf,
    sync::Arc,
    thread,
};

use axum::{
    response::Redirect,
    routing::{get, IntoMakeService},
    Router,
};
use clap::Parser;
use hyper::server::conn::AddrIncoming;
use listenfd::ListenFd;
use rand::random;
use serde::Serialize;
use serde_with::{serde_as, CommaSeparator, DisplayFromStr, StringWithSeparator};
use sysinfo::{RefreshKind, System, SystemExt};

use crate::{
    engine::Engine,
    ws::{Secret, SharedEngine},
};

/// External UCI engine provider for lichess.org.
#[derive(Debug, Parser)]
#[clap(version)]
pub struct Opt {
    /// UCI engine executable.
    engine: PathBuf,
    /// Bind server on this socket address.
    #[clap(long)]
    bind: Option<SocketAddr>,
    /// Overwrite engine name.
    #[clap(long)]
    name: Option<String>,
    /// Limit number of threads.
    #[clap(long)]
    max_threads: Option<usize>,
    /// Limit size of hash table (MiB).
    #[clap(long)]
    max_hash: Option<u64>,
    /// Provide secret token to use instead of a random one.
    #[clap(long)]
    secret: Option<String>,
    /// Promise that the selected engine is a recent official Stockfish
    /// release.
    #[clap(long, hide = true)]
    promise_official_stockfish: bool,
}

#[serde_as]
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ExternalWorkerOpts {
    url: String,
    secret: Secret,
    name: String,
    max_threads: usize,
    max_hash: u64,
    #[serde_as(as = "StringWithSeparator::<CommaSeparator, String>")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    variants: Vec<String>,
    #[serde_as(as = "DisplayFromStr")]
    #[serde(skip_serializing_if = "Not::not")]
    official_stockfish: bool,
}

impl ExternalWorkerOpts {
    pub fn registration_url(&self) -> String {
        format!(
            "https://lichess.org/analysis/external?{}",
            serde_urlencoded::to_string(&self).expect("serialize spec"),
        )
    }
}

fn available_memory() -> u64 {
    let sys = System::new_with_specifics(RefreshKind::new().with_memory());
    (sys.available_memory() / 1024).next_power_of_two() / 2
}

pub async fn make_server(
    opt: Opt,
    mut listen_fds: ListenFd,
) -> (
    ExternalWorkerOpts,
    hyper::Server<AddrIncoming, IntoMakeService<Router>>,
) {
    let (engine, info) = Engine::new(opt.engine).await.expect("spawn engine");
    let engine = Arc::new(SharedEngine::new(engine));

    let secret = Secret(
        opt.secret
            .unwrap_or_else(|| format!("{:032x}", random::<u128>())),
    );

    let listener = opt
        .bind
        .map(TcpListener::bind)
        .or_else(|| listen_fds.take_tcp_listener(0).transpose())
        .unwrap_or_else(|| TcpListener::bind("localhost:9670"))
        .expect("bind");

    let spec = ExternalWorkerOpts {
        url: format!("ws://{}/socket", listener.local_addr().expect("local addr")),
        secret: secret.clone(),
        max_threads: [
            info.max_threads.unwrap_or(1),
            opt.max_threads.unwrap_or(usize::MAX),
            thread::available_parallelism()
                .expect("available threads")
                .into(),
        ]
        .into_iter()
        .min()
        .unwrap(),
        max_hash: [
            info.max_hash.unwrap_or(16),
            opt.max_hash.unwrap_or(u64::MAX),
            available_memory(),
        ]
        .into_iter()
        .min()
        .unwrap(),
        variants: info.variants,
        name: opt
            .name
            .or(info.name)
            .unwrap_or_else(|| "remote-uci".to_owned()),
        official_stockfish: opt.promise_official_stockfish,
    };

    let app = Router::new()
        .route(
            "/",
            get({
                let spec = spec.clone();
                move || redirect(spec)
            }),
        )
        .route(
            "/socket",
            get({
                let engine = Arc::clone(&engine);
                let secret = secret;
                move |params, socket| ws::handler(engine, secret, params, socket)
            }),
        );

    (
        spec,
        axum::Server::from_tcp(listener)
            .expect("axum server")
            .serve(app.into_make_service()),
    )
}

async fn redirect(spec: ExternalWorkerOpts) -> Redirect {
    Redirect::to(&spec.registration_url())
}
