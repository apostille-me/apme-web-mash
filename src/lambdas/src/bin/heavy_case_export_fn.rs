#[path = "support/oracle_fn/mod.rs"]
mod oracle_fn;

use scintilla_route_lambdas::RouteSpec;

const ROUTE: RouteSpec = RouteSpec::new(
    "heavy_case_export",
    "POST",
    "/api/heavy/case-export",
    "case_id",
);

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    oracle_fn::run(ROUTE)
}
