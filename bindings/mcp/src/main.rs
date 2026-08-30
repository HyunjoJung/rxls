//! Standard-I/O entry point for the rxls MCP server.

use std::path::PathBuf;

use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleServer, ServiceExt};
use rxls_mcp::{BoundedLineReader, RxlsMcpServer, ServerConfig};

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("rxls-mcp: {message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let roots = parse_args()?;
    let config = ServerConfig::new(roots)?;
    let server = RxlsMcpServer::new(config);
    let input = BoundedLineReader::new(tokio::io::stdin());
    let transport = AsyncRwTransport::<RoleServer, _, _>::new_server(input, tokio::io::stdout());
    let service = server
        .serve(transport)
        .await
        .map_err(|source| format!("could not start MCP service: {source}"))?;
    service
        .waiting()
        .await
        .map_err(|source| format!("MCP service stopped with an error: {source}"))?;
    Ok(())
}

fn parse_args() -> Result<Vec<PathBuf>, String> {
    let mut args = std::env::args_os().skip(1);
    let mut roots = Vec::new();
    while let Some(argument) = args.next() {
        if argument == "--root" {
            let root = args
                .next()
                .ok_or_else(|| "--root requires a directory path".to_string())?;
            roots.push(PathBuf::from(root));
        } else if argument == "--help" || argument == "-h" {
            println!(
                "rxls-mcp {}\n\nUSAGE:\n    rxls-mcp [--root <DIRECTORY>]...\n\nDefaults to the current directory when no root is supplied.",
                env!("CARGO_PKG_VERSION")
            );
            std::process::exit(0);
        } else if argument == "--version" || argument == "-V" {
            println!("rxls-mcp {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        } else {
            return Err(format!(
                "unknown argument: {} (use --help)",
                argument.to_string_lossy()
            ));
        }
    }
    if roots.is_empty() {
        roots.push(
            std::env::current_dir()
                .map_err(|_| "could not read the process current directory".to_string())?,
        );
    }
    Ok(roots)
}
