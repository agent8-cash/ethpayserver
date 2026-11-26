//! Drives `RpcBlockSource` through its real public call path against two
//! hand-rolled JSON-RPC HTTP servers, rather than trusting a unit test that
//! never makes a real HTTP call. This is the acceptance test for the
//! multi-provider failover: provider 1 answers at startup, then 429s like a
//! provider whose free tier just ran out (the exact failure mode that took
//! testnet down on 2026-11-11 and again this week, with a single hardcoded
//! HTTP URL per chain and nothing to fail over to); provider 2 serves the
//! call that follows, and a provider that just failed sits out its cooldown
//! instead of being retried immediately.
//!
//! No mock-HTTP crate is in this workspace's dep tree for this crate, so the
//! server is a minimal hand-rolled one in the style of
//! `ws_pubsub_retry_escalation.rs`'s hand-rolled WS server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use evm::monitor::{BlockSource, RpcBlockSource, RpcSourceConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.expect("read request bytes");
        assert!(
            n > 0,
            "connection closed before full request headers arrived"
        );
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };

    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.expect("read request body");
        assert!(n > 0, "connection closed before full request body arrived");
        body.extend_from_slice(&chunk[..n]);
    }
    body
}

async fn write_json_response(stream: &mut TcpStream, body: &serde_json::Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write response");
    let _ = stream.shutdown().await;
}

async fn write_429(stream: &mut TcpStream) {
    let response =
        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write 429");
    let _ = stream.shutdown().await;
}

fn rpc_result(request: &serde_json::Value, result_hex: String) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request["id"].clone(),
        "result": result_hex,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn a_429_response_fails_over_to_the_next_configured_provider() {
    const CHAIN_ID: u64 = 11155111;
    const REAL_BLOCK_NUMBER: u64 = 424_242;

    // Provider "A": answers `eth_chainId` correctly (so startup succeeds and
    // it becomes the sticky primary), then 429s on every call after that.
    let a_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind A");
    let a_addr = a_listener.local_addr().expect("addr A");
    let a_requests = Arc::new(AtomicUsize::new(0));
    {
        let a_requests = Arc::clone(&a_requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = a_listener.accept().await else {
                    break;
                };
                a_requests.fetch_add(1, Ordering::SeqCst);
                let body = read_http_request(&mut socket).await;
                let req: serde_json::Value =
                    serde_json::from_slice(&body).expect("valid JSON-RPC request");
                if req["method"].as_str() == Some("eth_chainId") {
                    let result = rpc_result(&req, format!("{CHAIN_ID:#x}"));
                    write_json_response(&mut socket, &result).await;
                } else {
                    write_429(&mut socket).await;
                }
            }
        });
    }

    // Provider "B": answers everything correctly.
    let b_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind B");
    let b_addr = b_listener.local_addr().expect("addr B");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = b_listener.accept().await else {
                break;
            };
            let body = read_http_request(&mut socket).await;
            let req: serde_json::Value =
                serde_json::from_slice(&body).expect("valid JSON-RPC request");
            let result_hex = match req["method"].as_str() {
                Some("eth_chainId") => format!("{CHAIN_ID:#x}"),
                Some("eth_blockNumber") => format!("{REAL_BLOCK_NUMBER:#x}"),
                other => panic!("unexpected method hit the fallback provider: {other:?}"),
            };
            let result = rpc_result(&req, result_hex);
            write_json_response(&mut socket, &result).await;
        }
    });

    let config = RpcSourceConfig::http_only(format!("http://{a_addr}"), CHAIN_ID)
        .with_http_fallbacks([format!("http://{b_addr}")]);

    let source = RpcBlockSource::new(config)
        .await
        .expect("provider A answers eth_chainId, so startup succeeds on the primary");

    let block_number = source
        .get_block_number()
        .await
        .expect("provider A 429s on eth_blockNumber; the fallback must serve this call");

    assert_eq!(
        block_number, REAL_BLOCK_NUMBER,
        "the value returned must be the fallback's, not a fabricated success"
    );

    // Provider A was hit twice: once for the startup chain-id check, once
    // for the real call that 429'd.
    let a_requests_after_first_call = a_requests.load(Ordering::SeqCst);
    assert_eq!(a_requests_after_first_call, 2);

    source
        .get_block_number()
        .await
        .expect("the fallback keeps serving once it's the current provider");

    assert_eq!(
        a_requests.load(Ordering::SeqCst),
        a_requests_after_first_call,
        "a provider that just failed must sit out its cooldown rather than being retried immediately"
    );
}
