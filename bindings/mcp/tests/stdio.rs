//! End-to-end checks for the installed standard-I/O server boundary.

use std::process::Stdio;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;
use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::timeout;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

fn write_sample_xlsx(path: &std::path::Path) {
    let mut workbook = rxls::Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write_string(0, 0, "stdio journey");
    sheet.write_number(0, 1, 7.0);
    let bytes = workbook
        .to_xlsx_checked()
        .expect("author stdio journey workbook");
    std::fs::write(path, bytes).expect("write stdio journey workbook");
}

#[tokio::test]
async fn spawned_binary_serves_a_bounded_stdio_journey() {
    let root = TempDir::new().expect("create allowed root");
    let source = root.path().join("stdio.xlsx");
    write_sample_xlsx(&source);

    let mut child = Command::new(env!("CARGO_BIN_EXE_rxls-mcp"))
        .arg("--root")
        .arg(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn rxls-mcp binary");
    let stdout = child.stdout.take().expect("capture rxls-mcp stdout");
    let stdin = child.stdin.take().expect("capture rxls-mcp stdin");
    let transport = AsyncRwTransport::<RoleClient, _, _>::new_client(stdout, stdin);
    let client = timeout(STEP_TIMEOUT, ().serve(transport))
        .await
        .expect("MCP initialize timed out")
        .expect("initialize spawned MCP server");

    let tools = timeout(STEP_TIMEOUT, client.list_tools(None))
        .await
        .expect("tools/list timed out")
        .expect("list spawned MCP tools");
    assert_eq!(tools.tools.len(), 9);

    let arguments = json!({ "path": source.to_string_lossy() })
        .as_object()
        .expect("open arguments object")
        .clone();
    let opened = timeout(
        STEP_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("workbook_open").with_arguments(arguments)),
    )
    .await
    .expect("workbook_open timed out")
    .expect("open workbook through spawned MCP server");
    assert_ne!(opened.is_error, Some(true));
    let session_id = opened
        .structured_content
        .as_ref()
        .and_then(|value| value.get("session_id"))
        .and_then(serde_json::Value::as_str)
        .expect("spawned open result session ID")
        .to_string();

    let arguments = json!({
        "session_id": session_id.clone(),
        "sheet": "Data",
        "range": "A1:B1"
    })
    .as_object()
    .expect("read arguments object")
    .clone();
    let read = timeout(
        STEP_TIMEOUT,
        client
            .call_tool(CallToolRequestParams::new("workbook_read_range").with_arguments(arguments)),
    )
    .await
    .expect("workbook_read_range timed out")
    .expect("read workbook through spawned MCP server");
    assert_ne!(read.is_error, Some(true));
    assert_eq!(
        read.structured_content
            .as_ref()
            .and_then(|value| value.get("cell_count"))
            .and_then(serde_json::Value::as_u64),
        Some(2)
    );

    let arguments = json!({ "session_id": session_id })
        .as_object()
        .expect("close arguments object")
        .clone();
    let closed = timeout(
        STEP_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("workbook_close").with_arguments(arguments)),
    )
    .await
    .expect("workbook_close timed out")
    .expect("close workbook through spawned MCP server");
    assert_ne!(closed.is_error, Some(true));

    timeout(STEP_TIMEOUT, client.cancel())
        .await
        .expect("MCP client shutdown timed out")
        .expect("shut down spawned MCP client");
    let status = timeout(STEP_TIMEOUT, child.wait())
        .await
        .expect("rxls-mcp process exit timed out")
        .expect("wait for rxls-mcp process");
    assert!(status.success(), "rxls-mcp exited with {status}");
}
