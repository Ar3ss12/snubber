//! Example 2: Network / HTTP / gRPC client with Idempotency Key and Backpressure.
//!
//! Demonstrates the Network Decision Matrix:
//! - StaleEpoch: Stale session/packet -> Discard silently.
//! - Timeout at HeadersSent: Server might have processed it -> Retry ONLY with Idempotency-Key.
//! - BulkheadFull / RateLimited: Server pushes back -> Read `retry_after_ms()`, set Retry-After header.

use snubber::{EnterError, SnubberNode};

#[derive(Debug, PartialEq, Eq)]
enum HttpResponse {
    Ok200,
    RetryAfter429(u64),
    Ignored200,
    ResendWithIdempotencyKey(&'static str),
}

fn handle_http_request(
    node: &SnubberNode,
    epoch: u64,
    request_id: u64,
    idempotency_key: &'static str,
    simulate_hang_after_headers: bool,
) -> HttpResponse {
    match node.enter(epoch, request_id, || {
        // Rollback: cancel client connection / stream
    }) {
        Ok(mut guard) => {
            // Stage 1: Send request headers
            guard.checkpoint(1, "headers_sent");

            if simulate_hang_after_headers {
                let snap = guard.state_snapshot();
                eprintln!(
                    "[HTTP] Timeout after stage {} ({}): resending with Idempotency-Key: {}",
                    snap.stage,
                    snap.stage_label.unwrap_or(""),
                    idempotency_key
                );
                // Drop without commit records failure
                return HttpResponse::ResendWithIdempotencyKey(idempotency_key);
            }

            // Stage 2: Read response body
            guard.checkpoint(2, "body_received");
            guard.commit();
            HttpResponse::Ok200
        }
        Err(EnterError::StaleEpoch) => {
            // Out-of-order or duplicate packet from past epoch
            HttpResponse::Ignored200
        }
        Err(EnterError::BulkheadFull) | Err(EnterError::RateLimited) => {
            // Bulkhead is saturated: tell client to wait
            let wait_ms = 45; // or from backpressure hint
            HttpResponse::RetryAfter429(wait_ms)
        }
        Err(other) => panic!("unexpected enter error: {:?}", other),
    }
}

fn main() {
    println!("=== Snubber HTTP / Network Decision Matrix Example ===");

    let node = SnubberNode::builder("api_gateway").max_in_flight(2).build();

    // 1. Normal success
    let resp1 = handle_http_request(&node, 1, 10, "idem-key-1", false);
    assert_eq!(resp1, HttpResponse::Ok200);

    // 2. Stale epoch (epoch 1 is <= last_seen 1)
    let resp2 = handle_http_request(&node, 1, 11, "idem-key-2", false);
    assert_eq!(resp2, HttpResponse::Ignored200);

    // 3. Timeout after headers sent -> Must use same idempotency key on retry
    let resp3 = handle_http_request(&node, 2, 12, "idem-key-3", true);
    assert_eq!(resp3, HttpResponse::ResendWithIdempotencyKey("idem-key-3"));

    println!("HTTP idempotency flow verified successfully!");
}
