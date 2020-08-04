#[macro_use]
extern crate log;
#[macro_use]
extern crate prometheus;

mod utils;

use actix_web::{web, App, HttpResponse};
use commons::{metrics, policy};
use failure::{Error, Fallible, ResultExt};
use log::LevelFilter;
use prometheus::{Histogram, IntCounter, IntGauge};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use structopt::clap::{crate_name, crate_version};
use structopt::StructOpt;

/// Top-level log target for this application.
static APP_LOG_TARGET: &str = "fcos_policy_engine";

lazy_static::lazy_static! {
    static ref V1_GRAPH_INCOMING_REQS: IntCounter = register_int_counter!(opts!(
        "fcos_cincinnati_pe_v1_graph_incoming_requests_total",
        "Total number of incoming HTTP client request to /v1/graph"
    ))
    .unwrap();
    static ref UNIQUE_IDS: IntCounter = register_int_counter!(opts!(
        "fcos_cincinnati_pe_v1_graph_unique_uuids_total",
        "Total number of unique node UUIDs (per-instance Bloom filter)."
    ))
    .unwrap();
    static ref ROLLOUT_WARINESS: Histogram = register_histogram!(
        "fcos_cincinnati_pe_v1_graph_rollout_wariness",
        "Per-request rollout wariness.",
        prometheus::linear_buckets(0.0, 0.1, 11).unwrap()
    )
    .unwrap();
    // NOTE(lucab): alternatively this could come from the runtime library, see
    // https://prometheus.io/docs/instrumenting/writing_clientlibs/#process-metrics
    static ref PROCESS_START_TIME: IntGauge = register_int_gauge!(opts!(
        "process_start_time_seconds",
        "Start time of the process since unix epoch in seconds."
    )).unwrap();

}

fn main() -> Fallible<()> {
    let cli_opts = CliOptions::from_args();

    // Setup logging.
    env_logger::Builder::from_default_env()
        .format_timestamp(None)
        .format_module_path(false)
        .filter(Some(APP_LOG_TARGET), cli_opts.loglevel())
        .try_init()
        .context("failed to initialize logging")?;

    debug!("command-line options:\n{:#?}", cli_opts);

    let sys = actix::System::new("fcos_cincinnati_pe");

    let allowed_origins = vec!["https://builds.coreos.fedoraproject.org"];
    let node_population = Arc::new(cbloom::Filter::new(10 * 1024 * 1024, 1_000_000));
    let service_state = AppState {
        population: Arc::clone(&node_population),
    };

    let start_timestamp = chrono::Utc::now();
    PROCESS_START_TIME.set(start_timestamp.timestamp());
    info!("starting server ({} {})", crate_name!(), crate_version!());

    // Policy-engine service.
    let pe_service = service_state.clone();
    actix_web::HttpServer::new(move || {
        App::new()
            .wrap(commons::web::build_cors_middleware(&allowed_origins))
            .data(pe_service.clone())
            .route("/v1/graph", web::get().to(pe_serve_graph))
    })
    .bind((IpAddr::from(Ipv4Addr::UNSPECIFIED), 5051))?
    .run();

    // Policy-engine status service.
    let pe_status = service_state;
    actix_web::HttpServer::new(move || {
        App::new()
            .data(pe_status.clone())
            .route("/metrics", web::get().to(metrics::serve_metrics))
    })
    .bind((IpAddr::from(Ipv4Addr::UNSPECIFIED), 6061))?
    .run();

    sys.run()?;
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct AppState {
    population: Arc<cbloom::Filter>,
}

#[derive(Serialize, Deserialize)]
pub struct GraphQuery {
    basearch: Option<String>,
    stream: Option<String>,
    rollout_wariness: Option<String>,
    node_uuid: Option<String>,
}

pub(crate) async fn pe_serve_graph(
    data: actix_web::web::Data<AppState>,
    actix_web::web::Query(query): actix_web::web::Query<GraphQuery>,
) -> Result<HttpResponse, Error> {
    pe_record_metrics(&data, &query);

    let basearch = query
        .basearch
        .as_ref()
        .map(String::from)
        .unwrap_or_default();
    let stream = query.stream.as_ref().map(String::from).unwrap_or_default();
    trace!("graph query stream: {:#?}", stream);

    let wariness = compute_wariness(&query);
    ROLLOUT_WARINESS.observe(wariness);

    let cached_graph = utils::fetch_graph_from_gb(stream.clone(), basearch.clone()).await?;

    let throttled_graph = policy::throttle_rollouts(cached_graph, wariness);
    let final_graph = policy::filter_deadends(throttled_graph);

    let json =
        serde_json::to_string_pretty(&final_graph).map_err(|e| failure::format_err!("{}", e))?;
    let resp = HttpResponse::Ok()
        .content_type("application/json")
        .body(json);
    Ok(resp)
}

#[allow(clippy::let_and_return)]
fn compute_wariness(params: &GraphQuery) -> f64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    if let Ok(input) = params
        .rollout_wariness
        .as_ref()
        .map(String::from)
        .unwrap_or_default()
        .parse::<f64>()
    {
        let wariness = input.max(0.0).min(1.0);
        return wariness;
    }

    let uuid = params
        .node_uuid
        .as_ref()
        .map(String::from)
        .unwrap_or_default();
    let wariness = {
        // Left limit not included in range.
        const COMPUTED_MIN: f64 = 0.0 + 0.000_001;
        const COMPUTED_MAX: f64 = 1.0;
        let mut hasher = DefaultHasher::new();
        uuid.hash(&mut hasher);
        let digest = hasher.finish();
        // Scale down.
        let scaled = (digest as f64) / (std::u64::MAX as f64);
        // Clamp within limits.
        scaled.max(COMPUTED_MIN).min(COMPUTED_MAX)
    };

    wariness
}

pub(crate) fn pe_record_metrics(data: &AppState, query: &GraphQuery) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    V1_GRAPH_INCOMING_REQS.inc();

    if let Some(uuid) = &query.node_uuid {
        let mut hasher = DefaultHasher::default();
        uuid.hash(&mut hasher);
        let client_uuid = hasher.finish();
        if !data.population.maybe_contains(client_uuid) {
            data.population.insert(client_uuid);
            UNIQUE_IDS.inc();
        }
    }
}

/// CLI configuration options.
#[derive(Debug, StructOpt)]
pub(crate) struct CliOptions {
    /// Verbosity level (higher is more verbose).
    #[structopt(short = "v", parse(from_occurrences))]
    verbosity: u8,

    /// Path to configuration file.
    #[structopt(short = "c")]
    pub config_path: Option<String>,
}

impl CliOptions {
    /// Returns the log-level set via command-line flags.
    pub(crate) fn loglevel(&self) -> LevelFilter {
        match self.verbosity {
            0 => LevelFilter::Warn,
            1 => LevelFilter::Info,
            2 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        }
    }
}
