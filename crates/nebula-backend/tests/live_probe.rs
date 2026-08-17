//! 稼働中のバックエンドに外から接続し、開いているワークスペースを問い合わせる診断用テスト。
use nebula_protocol::{
    ClientMessage, FrameDecoder, PROTOCOL_VERSION, Request, RequestId, Response, ServerMessage,
    encode_frame,
};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[test]
#[ignore = "稼働中の GUI に対して手動で実行する診断"]
fn 稼働中のバックエンドが開いているワークスペースを列挙する() {
    let socket = nebula_protocol::default_socket_path();
    let mut stream = UnixStream::connect(&socket)
        .unwrap_or_else(|e| panic!("{} に接続できない: {e}", socket.display()));
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut decoder = FrameDecoder::new();

    let call = |stream: &mut UnixStream, decoder: &mut FrameDecoder, req: Request| -> Response {
        let id = RequestId::next();
        stream
            .write_all(&encode_frame(&ClientMessage::Request { id, request: req }).unwrap())
            .unwrap();
        let mut chunk = vec![0u8; 65536];
        loop {
            while let Ok(Some(msg)) = decoder.next_message::<ServerMessage>() {
                if let ServerMessage::Response { id: got, result } = msg
                    && got == id
                {
                    return result.expect("エラー応答");
                }
            }
            let n = stream.read(&mut chunk).unwrap();
            assert!(n > 0, "切断された");
            decoder.feed(&chunk[..n]);
        }
    };

    match call(&mut stream, &mut decoder, Request::Handshake { protocol_version: PROTOCOL_VERSION }) {
        Response::Handshake(info) => println!("握手 OK: backend pid {}", info.pid),
        other => panic!("{other:?}"),
    }
    match call(&mut stream, &mut decoder, Request::ListWorkspaces) {
        Response::Workspaces(list) => {
            println!("開いているワークスペース {} 件:", list.len());
            for w in &list {
                println!("  {} ({})", w.name, w.root.display());
            }
            assert!(!list.is_empty(), "GUI がワークスペースを開けていない");
        }
        other => panic!("{other:?}"),
    }
}
