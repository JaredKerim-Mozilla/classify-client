//! A server that tells clients what time it is and where they are in the world.
//!
#![deny(clippy::all)]

pub mod endpoints;
pub mod errors;
pub mod geoip;
pub mod logging;
pub mod metrics;
pub mod settings;
pub mod utils;

use crate::{
    endpoints::{classify, debug, dockerflow, EndpointState},
    errors::ClassifyError,
    geoip::GeoIp,
    settings::Settings,
};
use actix_web::{web, App};
use slog;
use std::sync::Arc;

const APP_NAME: &str = "classify-client";

fn main() -> Result<(), ClassifyError> {
    let Settings {
        debug,
        geoip_db_path,
        host,
        human_logs,
        metrics_target,
        port,
        trusted_proxy_list,
        version_file,
        ..
    } = Settings::load()?;

    let app_log = logging::get_logger("app", human_logs);

    let metrics = metrics::get_client(metrics_target, app_log.clone()).unwrap_or_else(|err| {
        panic!(format!(
            "Critical failure setting up metrics logging: {}",
            err
        ))
    });

    let state = EndpointState {
        geoip: Arc::new(
            GeoIp::builder()
                .path(geoip_db_path)
                .metrics(metrics.clone())
                .build()?,
        ),
        metrics,
        trusted_proxies: trusted_proxy_list,
        log: app_log.clone(),
        version_file,
    };

    let addr = format!("{}:{}", host, port);
    slog::info!(app_log, "starting server on https://{}", addr);

    actix_web::HttpServer::new(move || {
        let mut app = App::new()
            .data(state.clone())
            .wrap(metrics::ResponseTimer)
            .wrap(logging::RequestLogger)
            // API Endpoints
            .service(web::resource("/").route(web::get().to(classify::classify_client)))
            .service(
                web::resource("/api/v1/classify_client/")
                    .route(web::get().to(classify::classify_client)),
            )
            // Dockerflow Endpoints
            .service(
                web::resource("/__lbheartbeat__").route(web::get().to(dockerflow::lbheartbeat)),
            )
            .service(web::resource("/__heartbeat__").route(web::get().to(dockerflow::heartbeat)))
            .service(web::resource("/__version__").route(web::get().to(dockerflow::version)));

        if debug {
            app = app.service(web::resource("/debug").route(web::get().to(debug::debug_handler)));
        }

        app
    })
    .bind(&addr)?
    .run()?;

    Ok(())
}
