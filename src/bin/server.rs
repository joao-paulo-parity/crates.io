#![warn(clippy::all, rust_2018_idioms)]

use cargo_registry::{env_optional, metrics::LogEncoder, util::errors::AppResult, App, Env};
use std::{env, fs::File, process::Command, sync::Arc, time::Duration};

use conduit_hyper::Service;
use futures_util::future::FutureExt;
use prometheus::Encoder;
use reqwest::blocking::Client;
use std::io::{self, Write};
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{signal, SignalKind};
use tracing::Level;
use tracing_subscriber::{filter, prelude::*};

const CORE_THREADS: usize = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _sentry = cargo_registry::sentry::init();

    // Initialize logging

    let log_filter = env::var("RUST_LOG")
        .unwrap_or_default()
        .parse::<filter::Targets>()
        .expect("Invalid RUST_LOG value");

    let sentry_filter = filter::Targets::new().with_default(Level::INFO);

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(log_filter))
        .with(sentry::integrations::tracing::layer().with_filter(sentry_filter))
        .init();

    let config = cargo_registry::config::Server::default();
    let env = config.env();
    let client = Client::new();
    let app = Arc::new(App::new(config, Some(client)));

    {
        use cargo_registry::models::NewUser;
        use cargo_registry::schema::api_tokens;
        use diesel::prelude::*;
        use cargo_registry::db::oneoff_connection_with_config;
        let conn = oneoff_connection_with_config(&app.config.db).unwrap();
        let user = NewUser {
                gh_id: 1,
                gh_login: "login",
                name: None,
                gh_avatar: None,
                gh_access_token: "access_token".into()
            }
            .create_or_update(None, &app.emails, &conn)
            .unwrap();
        let api_token = env::var("API_TOKEN").unwrap();
        let api_token_bytes = api_token.as_bytes().into_sql::<diesel::sql_types::Binary>();
        diesel::insert_into(api_tokens::table)
            .values((
                api_tokens::user_id.eq(user.id),
                api_tokens::name.eq("foo"),
                api_tokens::token.eq(api_token_bytes),
            ))
            .execute(&conn).unwrap();
    }

    // Start the background thread periodically persisting download counts to the database.
    downloads_counter_thread(app.clone());

    // Start the background thread periodically logging instance metrics.
    log_instance_metrics_thread(app.clone());

    let handler = cargo_registry::build_handler(app.clone());

    let heroku = dotenv::var("HEROKU").is_ok();
    let fastboot = dotenv::var("USE_FASTBOOT").is_ok();
    let dev_docker = dotenv::var("DEV_DOCKER").is_ok();

    let ip = if dev_docker {
        [0, 0, 0, 0]
    } else {
        [127, 0, 0, 1]
    };
    let port = match (heroku, env_optional("PORT")) {
        (false, Some(port)) => port,
        _ => 8888,
    };

    let threads = dotenv::var("SERVER_THREADS")
        .map(|s| s.parse().expect("SERVER_THREADS was not a valid number"))
        .unwrap_or_else(|_| match env {
            Env::Development => 5,
            // A large default because this can be easily changed via env and in production we
            // want the logging middleware to accurately record the start time.
            _ => 500,
        });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(CORE_THREADS)
        .max_blocking_threads(threads as usize)
        .build()
        .unwrap();

    let handler = Arc::new(conduit_hyper::BlockingHandler::new(handler));
    let make_service =
        hyper::service::make_service_fn(move |socket: &hyper::server::conn::AddrStream| {
            let addr = socket.remote_addr();
            let handler = handler.clone();
            async move { Service::from_blocking(handler, addr) }
        });

    let (addr, server) = rt.block_on(async {
        let server = hyper::Server::bind(&(ip, port).into()).serve(make_service);

        // When the user configures PORT=0 the operating system will allocate a random unused port.
        // This fetches that random port and uses it to display the the correct url later.
        let addr = server.local_addr();

        let mut sig_int = signal(SignalKind::interrupt())?;
        let mut sig_term = signal(SignalKind::terminate())?;
        let server = server.with_graceful_shutdown(async move {
            // Wait for either signal
            tokio::select! {
                _ = sig_int.recv().fuse() => {},
                _ = sig_term.recv().fuse() => {},
            };
            tokio::io::stdout()
                .write_all(b"Starting graceful shutdown\n")
                .await
                .ok();
        });

        Ok::<_, io::Error>((addr, server))
    })?;

    // Do not change this line! Removing the line or changing its contents in any way will break
    // the test suite :)
    println!("Listening at http://{addr}");

    // Creating this file tells heroku to tell nginx that the application is ready
    // to receive traffic.
    if heroku {
        let path = if fastboot {
            "/tmp/backend-initialized"
        } else {
            "/tmp/app-initialized"
        };
        println!("Writing to {path}");
        File::create(path).unwrap();

        // Launch nginx via the Heroku nginx buildpack
        // `wait()` is never called on the child process, but it should be okay to leave a zombie
        // process around on shutdown when Heroku is tearing down the entire container anyway.
        Command::new("./script/start-web.sh")
            .spawn()
            .expect("Couldn't spawn nginx");
    }

    // Block the main thread until the server has shutdown
    rt.block_on(server)?;

    println!("Persisting remaining downloads counters");
    match app.downloads_counter.persist_all_shards(&app) {
        Ok(stats) => stats.log(),
        Err(err) => println!("downloads_counter error: {err}"),
    }

    println!("Server has gracefully shutdown!");
    Ok(())
}

fn downloads_counter_thread(app: Arc<App>) {
    let interval = Duration::from_millis(
        (app.config.downloads_persist_interval_ms / app.downloads_counter.shards_count()) as u64,
    );

    std::thread::spawn(move || loop {
        std::thread::sleep(interval);

        match app.downloads_counter.persist_next_shard(&app) {
            Ok(stats) => stats.log(),
            Err(err) => println!("downloads_counter error: {err}"),
        }
    });
}

fn log_instance_metrics_thread(app: Arc<App>) {
    // Only run the thread if the configuration is provided
    let interval = if let Some(secs) = app.config.instance_metrics_log_every_seconds {
        Duration::from_secs(secs)
    } else {
        return;
    };

    std::thread::spawn(move || loop {
        if let Err(err) = log_instance_metrics_inner(&app) {
            eprintln!("log_instance_metrics error: {err}");
        }
        std::thread::sleep(interval);
    });
}

fn log_instance_metrics_inner(app: &App) -> AppResult<()> {
    let families = app.instance_metrics.gather(app)?;

    let mut stdout = std::io::stdout();
    LogEncoder::new().encode(&families, &mut stdout)?;
    stdout.flush()?;

    Ok(())
}
