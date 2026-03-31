//! JSON-RPC request handlers for the MCP server.

use crate::tools::mcp_protocol::*;
use crate::tools::traits::Tool;
use serde_json::{json, Value};

/// Route a JSON-RPC request to the appropriate handler.
pub async fn handle_request(
    req: &JsonRpcRequest,
    tools: &[Box<dyn Tool>],
) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => handle_initialize(req),
        "notifications/initialized" => notification_ack(),
        "tools/list" => handle_tools_list(req, tools),
        "tools/call" => handle_tools_call(req, tools).await,
        "resources/list" => handle_resources_list(req),
        "resources/read" => handle_resources_read(req),
        "prompts/list" => handle_prompts_list(req),
        "prompts/get" => handle_prompts_get(req),
        _ => method_not_found(req),
    }
}

fn handle_initialize(req: &JsonRpcRequest) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: req.id.clone(),
        result: Some(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "serverInfo": {
                "name": "zeroclaw",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "tools": {},
                "resources": {},
                "prompts": {},
            }
        })),
        error: None,
    }
}

fn handle_tools_list(req: &JsonRpcRequest, tools: &[Box<dyn Tool>]) -> JsonRpcResponse {
    let tool_defs: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name(),
                "description": t.description(),
                "inputSchema": t.parameters_schema(),
            })
        })
        .collect();

    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: req.id.clone(),
        result: Some(json!({ "tools": tool_defs })),
        error: None,
    }
}

async fn handle_tools_call(req: &JsonRpcRequest, tools: &[Box<dyn Tool>]) -> JsonRpcResponse {
    let params = match &req.params {
        Some(p) => p,
        None => return error_response(req, INVALID_PARAMS, "Missing params"),
    };

    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let tool = match tools.iter().find(|t| t.name() == tool_name) {
        Some(t) => t,
        None => {
            return error_response(req, METHOD_NOT_FOUND, &format!("Unknown tool: {tool_name}"))
        }
    };

    match tool.execute(arguments).await {
        Ok(result) => JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: req.id.clone(),
            result: Some(json!({
                "content": [{
                    "type": "text",
                    "text": result.output,
                }],
                "isError": !result.success,
            })),
            error: None,
        },
        Err(e) => JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: req.id.clone(),
            result: Some(json!({
                "content": [{
                    "type": "text",
                    "text": format!("Error: {e}"),
                }],
                "isError": true,
            })),
            error: None,
        },
    }
}

fn handle_resources_list(req: &JsonRpcRequest) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: req.id.clone(),
        result: Some(json!({
            "resources": [
                {
                    "uri": "zeroclaw://version",
                    "name": "ZeroClaw Version",
                    "description": "Current ZeroClaw version and build info",
                    "mimeType": "application/json",
                }
            ]
        })),
        error: None,
    }
}

fn handle_resources_read(req: &JsonRpcRequest) -> JsonRpcResponse {
    let uri = req
        .params
        .as_ref()
        .and_then(|p| p.get("uri"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let content = match uri {
        "zeroclaw://version" => json!({
            "version": env!("CARGO_PKG_VERSION"),
            "name": "zeroclaw",
        })
        .to_string(),
        _ => return error_response(req, INVALID_PARAMS, &format!("Unknown resource: {uri}")),
    };

    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: req.id.clone(),
        result: Some(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": content,
            }]
        })),
        error: None,
    }
}

fn handle_prompts_list(req: &JsonRpcRequest) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: req.id.clone(),
        result: Some(json!({ "prompts": [] })),
        error: None,
    }
}

fn handle_prompts_get(req: &JsonRpcRequest) -> JsonRpcResponse {
    error_response(req, INVALID_PARAMS, "No prompts available")
}

fn notification_ack() -> JsonRpcResponse {
    // Notifications don't get responses, but we return an empty one for the router
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: None,
        result: None,
        error: None,
    }
}

fn method_not_found(req: &JsonRpcRequest) -> JsonRpcResponse {
    error_response(
        req,
        METHOD_NOT_FOUND,
        &format!("Unknown method: {}", req.method),
    )
}

fn error_response(req: &JsonRpcRequest, code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: req.id.clone(),
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::traits::{Tool, ToolResult};
    use async_trait::async_trait;

    struct MockTool;

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            "mock_tool"
        }
        fn description(&self) -> &str {
            "A mock tool for testing"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                }
            })
        }
        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            let input = args
                .get("input")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            Ok(ToolResult {
                success: true,
                output: format!("executed: {input}"),
                error: None,
            })
        }
    }

    fn make_request(method: &str, params: Option<Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(1)),
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn initialize_returns_protocol_version_and_capabilities() {
        let req = make_request("initialize", None);
        let resp = handle_initialize(&req);

        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"]["resources"].is_object());
        assert!(result["capabilities"]["prompts"].is_object());
        assert_eq!(result["serverInfo"]["name"], "zeroclaw");
    }

    #[test]
    fn tools_list_returns_registered_tools() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool)];
        let req = make_request("tools/list", None);
        let resp = handle_tools_list(&req, &tools);

        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        let tools_arr = result["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["name"], "mock_tool");
        assert_eq!(tools_arr[0]["description"], "A mock tool for testing");
        assert!(tools_arr[0]["inputSchema"].is_object());
    }

    #[tokio::test]
    async fn tools_call_executes_known_tool() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool)];
        let req = make_request(
            "tools/call",
            Some(json!({
                "name": "mock_tool",
                "arguments": { "input": "hello" }
            })),
        );
        let resp = handle_tools_call(&req, &tools).await;

        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["content"][0]["text"], "executed: hello");
        assert_eq!(result["isError"], false);
    }

    #[tokio::test]
    async fn tools_call_returns_error_for_unknown_tool() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool)];
        let req = make_request(
            "tools/call",
            Some(json!({
                "name": "nonexistent",
                "arguments": {}
            })),
        );
        let resp = handle_tools_call(&req, &tools).await;

        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert_eq!(err.code, METHOD_NOT_FOUND);
        assert!(err.message.contains("nonexistent"));
    }

    #[tokio::test]
    async fn tools_call_returns_error_for_missing_params() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool)];
        let req = make_request("tools/call", None);
        let resp = handle_tools_call(&req, &tools).await;

        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let tools: Vec<Box<dyn Tool>> = vec![];
        let req = make_request("unknown/method", None);
        let resp = handle_request(&req, &tools).await;

        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn resources_list_returns_version_resource() {
        let req = make_request("resources/list", None);
        let resp = handle_resources_list(&req);

        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        let resources = result["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0]["uri"], "zeroclaw://version");
    }

    #[test]
    fn resources_read_returns_version_info() {
        let req = make_request(
            "resources/read",
            Some(json!({ "uri": "zeroclaw://version" })),
        );
        let resp = handle_resources_read(&req);

        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        let contents = result["contents"].as_array().unwrap();
        assert_eq!(contents[0]["uri"], "zeroclaw://version");
        assert_eq!(contents[0]["mimeType"], "application/json");
    }

    #[test]
    fn resources_read_unknown_uri_returns_error() {
        let req = make_request(
            "resources/read",
            Some(json!({ "uri": "zeroclaw://unknown" })),
        );
        let resp = handle_resources_read(&req);

        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
    }

    #[test]
    fn prompts_list_returns_empty() {
        let req = make_request("prompts/list", None);
        let resp = handle_prompts_list(&req);

        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        let prompts = result["prompts"].as_array().unwrap();
        assert!(prompts.is_empty());
    }

    #[test]
    fn prompts_get_returns_error() {
        let req = make_request("prompts/get", Some(json!({ "name": "anything" })));
        let resp = handle_prompts_get(&req);

        assert!(resp.error.is_some());
    }

    #[test]
    fn notification_ack_has_no_id_or_result() {
        let resp = notification_ack();
        assert!(resp.id.is_none());
        assert!(resp.result.is_none());
        assert!(resp.error.is_none());
    }
}
