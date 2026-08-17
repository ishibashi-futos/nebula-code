//! codex app-server との JSON-RPC のフレーミングと振り分け。
//!
//! app-server は 1 行 1 JSON (NDJSON) で読み書きする。LSP のような Content-Length
//! ヘッダは使わない。これは codex 0.147.0 の `codex app-server` を実際に起動して
//! 確認した挙動で、`JSONRPCMessage.json` にもヘッダの定義は無い。

use serde_json::{Value, json};

/// 受け取った 1 行の役割。
///
/// JSON-RPC は 1 本のストリームに応答・要求・通知が混ざって流れてくるので、
/// 読み取りループが最初にここで振り分ける。
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// こちらの要求に対する成功応答。
    Response { id: i64, result: Value },
    /// こちらの要求に対する失敗応答。
    Failure { id: i64, message: String },
    /// サーバーからの要求。応答を返さないと codex はそこで待ち続ける。
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// 一方向の通知。
    Notification { method: String, params: Value },
}

/// 1 行を `Incoming` に振り分ける。解釈できない行は `None`。
///
/// codex は `emittedAtMs` のように仕様書に無い項目を足してくるため、
/// 構造体へのデシリアライズではなく `Value` のまま必要な項目だけを見る。
pub fn classify(line: &str) -> Option<Incoming> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id");

    match (id, method) {
        // id と method が揃っていればサーバーからの要求。
        (Some(id), Some(method)) => Some(Incoming::ServerRequest {
            id: id.clone(),
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(id), None) => {
            let id = id.as_i64()?;
            match value.get("error") {
                Some(error) => Some(Incoming::Failure {
                    id,
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("codex app-server がエラーを返しました")
                        .to_string(),
                }),
                None => Some(Incoming::Response {
                    id,
                    result: value.get("result").cloned().unwrap_or(Value::Null),
                }),
            }
        }
        (None, Some(method)) => Some(Incoming::Notification {
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, None) => None,
    }
}

/// 送信する 1 行を組み立てる。末尾の改行までを含む。
pub fn request_frame(id: i64, method: &str, params: Value) -> String {
    line(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
}

pub fn notification_frame(method: &str, params: Value) -> String {
    line(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
}

pub fn response_frame(id: &Value, result: Value) -> String {
    line(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// 対応していないサーバー要求へ返す失敗応答。
///
/// 黙殺するとサーバーが応答待ちで固まるため、必ず何かを返す。
/// -32601 は JSON-RPC の "Method not found"。
pub fn method_not_found_frame(id: &Value, method: &str) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": format!("Nebula は {method} に対応していません") },
    }))
}

fn line(value: Value) -> String {
    let mut text = value.to_string();
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 応答を振り分けられる() {
        let line = r#"{"id":1,"result":{"userAgent":"nebula/0.147.0"}}"#;
        let Some(Incoming::Response { id, result }) = classify(line) else {
            panic!("応答として振り分けられませんでした");
        };
        assert_eq!(id, 1);
        assert_eq!(result["userAgent"], "nebula/0.147.0");
    }

    #[test]
    fn 失敗応答を振り分けられる() {
        let line = r#"{"id":7,"error":{"code":-32602,"message":"bad params"}}"#;
        assert_eq!(
            classify(line),
            Some(Incoming::Failure {
                id: 7,
                message: "bad params".to_string(),
            })
        );
    }

    #[test]
    fn 通知を振り分けられる() {
        // 採取した実際の 1 行。仕様書に無い emittedAtMs が付くことに注意。
        let line = r#"{"method":"thread/status/changed","params":{"threadId":"019f","status":{"type":"idle"}},"emittedAtMs":1786400491}"#;
        let Some(Incoming::Notification { method, params }) = classify(line) else {
            panic!("通知として振り分けられませんでした");
        };
        assert_eq!(method, "thread/status/changed");
        assert_eq!(params["threadId"], "019f");
    }

    #[test]
    fn サーバー要求は応答と区別される() {
        let line = r#"{"id":"req-1","method":"item/commandExecution/requestApproval","params":{"itemId":"exec-1"}}"#;
        let Some(Incoming::ServerRequest { id, method, .. }) = classify(line) else {
            panic!("サーバー要求として振り分けられませんでした");
        };
        assert_eq!(id, Value::String("req-1".into()));
        assert_eq!(method, "item/commandExecution/requestApproval");
    }

    #[test]
    fn 壊れた行は無視される() {
        assert_eq!(classify(""), None);
        assert_eq!(classify("これは JSON ではない"), None);
        assert_eq!(classify(r#"{"result":1}"#), None);
    }

    #[test]
    fn 送信フレームは一行で終わる() {
        let frame = request_frame(3, "model/list", json!({}));
        assert!(frame.ends_with('\n'));
        assert_eq!(frame.matches('\n').count(), 1);
        let parsed: Value = serde_json::from_str(frame.trim()).unwrap();
        assert_eq!(parsed["id"], 3);
        assert_eq!(parsed["method"], "model/list");
    }

    #[test]
    fn 未対応のサーバー要求にはエラーを返す() {
        let frame = method_not_found_frame(&json!(12), "attestation/generate");
        let parsed: Value = serde_json::from_str(frame.trim()).unwrap();
        assert_eq!(parsed["id"], 12);
        assert_eq!(parsed["error"]["code"], -32601);
    }
}
