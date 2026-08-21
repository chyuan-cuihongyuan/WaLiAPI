fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    if let Err(error) = tokio::runtime::Runtime::new()
        .expect("create Tokio runtime")
        .block_on(waliapi_lib::run_headless())
    {
        eprintln!("waliapi-server: {error}");
        std::process::exit(1);
    }
}
