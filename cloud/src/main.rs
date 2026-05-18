use std::net::SocketAddr;

use accountir_cloud::{config::Config, db, http};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "accountir_cloud=debug,tower_http=info,sqlx=warn".into()),
        )
        .init();

    let config = Config::from_env()?;
    let pool = db::connect(&config).await?;
    db::migrate(&pool).await?;

    let email = accountir_cloud::email::EmailClient::from_env();
    let app = http::router(http::AppState { pool, config: config.clone(), email });

    let addr: SocketAddr = config.bind_addr.parse()?;
    tracing::info!(%addr, "starting accountir-cloud");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
