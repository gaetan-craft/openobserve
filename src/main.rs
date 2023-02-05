use actix_web::{middleware, web, App, HttpServer};
use actix_web_opentelemetry::RequestTracing;
use actix_web_prometheus::PrometheusMetricsBuilder;
use opentelemetry::sdk::propagation::TraceContextPropagator;
use opentelemetry::sdk::{trace as sdktrace, Resource};
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceServiceServer;
use prometheus::{opts, GaugeVec};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tonic::codec::CompressionEncoding;
use tracing_subscriber::prelude::*;
use tracing_subscriber::Registry;
use zinc_observe::handler::grpc::auth::check_auth;
use zinc_observe::handler::grpc::cluster_rpc::event_server::EventServer;
use zinc_observe::handler::grpc::cluster_rpc::search_server::SearchServer;
use zinc_observe::handler::grpc::request::{event::Eventer, search::Searcher, traces::TraceServer};
use zinc_observe::handler::http::router::get_routes;
use zinc_observe::infra::cluster;
use zinc_observe::infra::config::CONFIG;
use zinc_observe::service::router;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    if CONFIG.common.tracing_enabled {
        let service_name = format!("zinc-observe/{}", CONFIG.common.instance_name);
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let mut headers = HashMap::new();
        headers.insert(
            CONFIG.common.tracing_header_key.clone(),
            CONFIG.common.tracing_header_value.clone(),
        );
        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .http()
                    .with_endpoint(&CONFIG.common.otel_otlp_url)
                    .with_headers(headers),
            )
            .with_trace_config(sdktrace::config().with_resource(Resource::new(vec![
                KeyValue::new("service.name", service_name),
                KeyValue::new("deployment_type", "k8s"),
            ])))
            .install_batch(opentelemetry::runtime::Tokio)?;

        Registry::default()
            .with(tracing_subscriber::EnvFilter::new(&CONFIG.log.level))
            .with(tracing_subscriber::fmt::layer())
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .init();
    } else {
        env_logger::init_from_env(env_logger::Env::new().default_filter_or(&CONFIG.log.level));
    }

    // init jobs
    // it must be initialized before the server starts
    let (tx, rx) = oneshot::channel();
    tokio::task::spawn(async move {
        cluster::register_and_keepalive()
            .await
            .unwrap_or_else(|e| panic!("cluster init failed: {}", e));
        zinc_observe::job::init()
            .await
            .unwrap_or_else(|e| panic!("job init failed: {}", e));
        tx.send(true).unwrap();
    });

    // gRPC server
    rx.await.unwrap();
    if !router::is_router() {
        let gaddr: SocketAddr = format!("0.0.0.0:{}", CONFIG.grpc.port).parse()?;
        let searcher = Searcher::default();
        let search_svc = SearchServer::new(searcher)
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip);
        let eventer = Eventer::default();
        let event_svc = EventServer::new(eventer)
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip);
        let tracer = TraceServer::default();
        let trace_svc = TraceServiceServer::new(tracer);

        tokio::task::spawn(async move {
            log::info!("starting gRPC server at {}", gaddr);
            tonic::transport::Server::builder()
                .layer(tonic::service::interceptor(check_auth))
                .add_service(search_svc)
                .add_service(event_svc)
                .add_service(trace_svc)
                .serve(gaddr)
                .await
                .unwrap_or_else(|e| panic!("gRPC server failed: {}", e));
        });
    }

    // let node online
    let _ = cluster::set_online().await;

    // metrics
    let mut labels = HashMap::new();
    labels.insert("hostname".to_string(), CONFIG.common.instance_name.clone());
    labels.insert("role".to_string(), CONFIG.common.node_role.clone());
    let prometheus = PrometheusMetricsBuilder::new("zinc_observe")
        .endpoint("/metrics")
        .const_labels(labels)
        .build()
        .unwrap();
    let stats_opts =
        opts!("ingest_stats", "Summary ingestion stats metric").namespace("zinc_observe");
    let stats = GaugeVec::new(stats_opts, &["org", "name", "field"]).unwrap();
    prometheus
        .registry
        .register(Box::new(stats.clone()))
        .unwrap();

    // HTTP server
    let thread_id = Arc::new(AtomicU8::new(0));
    let haddr: SocketAddr = format!("0.0.0.0:{}", CONFIG.http.port).parse()?;
    if router::is_router() {
        HttpServer::new(move || {
            log::info!("starting HTTP server at: {}", haddr);
            App::new()
                .default_service(web::route().to(router::dispatch))
                .app_data(web::JsonConfig::default().limit(CONFIG.limit.req_json_limit))
                .app_data(web::PayloadConfig::new(CONFIG.limit.req_payload_limit)) // size is in bytes
                .app_data(web::Data::new(
                    awc::Client::builder()
                        .timeout(Duration::from_secs(CONFIG.route.timeout))
                        .finish(),
                ))
                .wrap(middleware::Logger::default())
                .wrap(RequestTracing::new())
                .wrap(prometheus.clone())
        })
        .bind(haddr)?
        .run()
        .await?;
    } else {
        HttpServer::new(move || {
            let local_id = thread_id.load(Ordering::SeqCst) as usize;
            if CONFIG.common.feature_per_thread_lock {
                thread_id.fetch_add(1, Ordering::SeqCst);
            }
            log::info!(
                "starting HTTP server at: {}, thread_id: {}",
                haddr,
                local_id
            );
            App::new()
                .configure(get_routes)
                .app_data(web::JsonConfig::default().limit(CONFIG.limit.req_json_limit))
                .app_data(web::PayloadConfig::new(CONFIG.limit.req_payload_limit)) // size is in bytes
                .app_data(web::Data::new(stats.clone()))
                .app_data(web::Data::new(local_id))
                .wrap(middleware::Logger::default())
                .wrap(RequestTracing::new())
                .wrap(prometheus.clone())
        })
        .bind(haddr)?
        .run()
        .await?;
    };

    // leave the cluster
    let _ = cluster::leave().await;

    log::info!("server stopped");

    Ok(())
}