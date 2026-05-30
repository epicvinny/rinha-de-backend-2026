//! Fraud-score classifier extracted from the `api` crate so it can be linked
//! into the C epoll reactor (`reactor.c`) via a stable C ABI while remaining the
//! single source of truth for the Rust `api` binary.
//!
//! Scoring semantics match `tree_only` mode in `api::score_body_fast_bucket`:
//! `classify_approved(body)` -> `Some(true)` (approved) / `Some(false)` (denied)
//! / `None` (unparseable body -> HTTP 400).

pub mod classifier;
pub mod tree_model;

pub use classifier::classify_approved;

/// Score one fraud-score request body for the C reactor.
///
/// Returns the response bucket: `0` = approved, `5` = denied, `255` = parse
/// error (the caller should emit HTTP 400). Pure and allocation-free; the
/// release profile is `panic = "abort"`, so this never unwinds across the FFI
/// boundary.
///
/// # Safety
/// `body_ptr` must point to `body_len` readable bytes (or be null, which returns
/// 255). The slice is only read for the duration of the call.
#[no_mangle]
pub extern "C" fn rinha_score_body(body_ptr: *const u8, body_len: usize) -> u8 {
    if body_ptr.is_null() || body_len == 0 {
        return 255;
    }
    let body = unsafe { std::slice::from_raw_parts(body_ptr, body_len) };
    match classifier::classify_approved(body) {
        Some(true) => 0,
        Some(false) => 5,
        None => 255,
    }
}

#[cfg(test)]
mod ffi_tests {
    use super::*;

    fn score(body: &[u8]) -> u8 {
        rinha_score_body(body.as_ptr(), body.len())
    }

    #[test]
    fn ffi_matches_classify_approved() {
        let body = br#"{"transaction":{"amount":41.12,"installments":2,"requested_at":"2026-03-11T18:45:53Z"},"customer":{"avg_amount":82.24,"tx_count_24h":3,"known_merchants":["MERC-003","MERC-016"]},"merchant":{"id":"MERC-016","mcc":"5411","avg_amount":60.25},"terminal":{"is_online":false,"card_present":true,"km_from_home":29.23},"last_transaction":null}"#;
        let expected = match classify_approved(body) {
            Some(true) => 0u8,
            Some(false) => 5u8,
            None => 255u8,
        };
        assert_eq!(score(body), expected);
    }

    #[test]
    fn ffi_null_and_empty_are_parse_errors() {
        assert_eq!(rinha_score_body(std::ptr::null(), 0), 255);
        assert_eq!(score(b""), 255);
        assert_eq!(score(b"not json"), 255);
    }
}
