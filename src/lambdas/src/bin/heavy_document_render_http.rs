use scintilla_route_lambdas::{run_http, RouteSpec};

const ROUTE: RouteSpec = RouteSpec::new(
    "heavy_document_render",
    "POST",
    "/api/heavy/document-render",
    "document_id",
);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_http(ROUTE).await
}
