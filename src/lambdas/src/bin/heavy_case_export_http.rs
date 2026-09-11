use scintilla_route_lambdas::{run_http, RouteSpec};

const ROUTE: RouteSpec = RouteSpec::new(
    "heavy_case_export",
    "POST",
    "/api/heavy/case-export",
    "case_id",
);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_http(ROUTE).await
}
