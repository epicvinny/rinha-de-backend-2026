use crate::cell::{DAY_Q, HOUR_Q};
use crate::normalization::Constants;
use crate::types::Vec16I;
use crate::vectorize::{clamp01, fast_parse_mcc, parse_iso8601};

#[inline]
fn quantize01(value: f64) -> i16 {
    (clamp01(value) * 10000.0).round() as i16
}

pub fn parse_payload_to_i16_and_key(body: &[u8], c: &Constants) -> Option<(Vec16I, u16)> {
    let mut pos = 0usize;

    pos = find_after(body, b"\"transaction\":{\"amount\":", pos)?;
    let (amount, next) = parse_f64_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"installments\":", pos)?;
    let (installments, next) = parse_u32_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"requested_at\":\"", pos)?;
    let (requested_at, next) = parse_str_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b"},\"customer\":{\"avg_amount\":", pos)?;
    let (customer_avg_amount, next) = parse_f64_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"tx_count_24h\":", pos)?;
    let (tx_count_24h, next) = parse_u32_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"known_merchants\":[", pos)?;
    let known_start = pos;
    let known_end = find_byte(body, b']', known_start)?;
    let known_merchants = &body[known_start..known_end];
    pos = known_end + 1;

    pos = expect_after(body, b"},\"merchant\":{\"id\":\"", pos)?;
    let (merchant_id, next) = parse_str_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"mcc\":\"", pos)?;
    let (merchant_mcc, next) = parse_str_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"avg_amount\":", pos)?;
    let (merchant_avg_amount, next) = parse_f64_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b"},\"terminal\":{\"is_online\":", pos)?;
    let (is_online, next) = parse_bool_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"card_present\":", pos)?;
    let (card_present, next) = parse_bool_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"km_from_home\":", pos)?;
    let (km_from_home, next) = parse_f64_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b"},\"last_transaction\":", pos)?;
    let has_last_transaction = !body.get(pos..)?.starts_with(b"null");

    let requested = parse_iso8601(requested_at)?;
    let mut out = [0i16; 16];

    out[0] = quantize01(amount / c.max_amount);
    out[1] = quantize01(installments as f64 / c.max_installments);
    out[2] = if customer_avg_amount == 0.0 {
        10000
    } else {
        quantize01((amount / customer_avg_amount) / c.amount_vs_avg_ratio)
    };
    out[3] = HOUR_Q[requested.hour as usize];
    out[4] = DAY_Q[requested.day_of_week as usize];

    if has_last_transaction {
        pos = expect_after(body, b"{\"timestamp\":\"", pos)?;
        let (last_timestamp, next) = parse_str_at(body, pos)?;
        pos = next;

        pos = expect_after(body, b",\"km_from_current\":", pos)?;
        let (km_from_current, _) = parse_f64_at(body, pos)?;
        let last = parse_iso8601(last_timestamp)?;
        let minutes = (requested.epoch_seconds - last.epoch_seconds).abs() as f64 / 60.0;
        out[5] = quantize01(minutes / c.max_minutes);
        out[6] = quantize01(km_from_current / c.max_km);
    } else {
        out[5] = -10000;
        out[6] = -10000;
    }

    out[7] = quantize01(km_from_home / c.max_km);
    out[8] = quantize01(tx_count_24h as f64 / c.max_tx_count_24h);

    let is_online_val = if is_online { 10000 } else { 0 };
    let card_present_val = if card_present { 10000 } else { 0 };
    out[9] = is_online_val;
    out[10] = card_present_val;

    let known = known_merchants_contains(known_merchants, merchant_id.as_bytes());
    out[11] = if known { 0 } else { 10000 };

    let mcc_val = fast_parse_mcc(merchant_mcc);
    out[12] = if mcc_val < 10000 {
        c.mcc_risk_i16[mcc_val]
    } else {
        5000
    };
    out[13] = quantize01(merchant_avg_amount / c.max_merchant_avg_amount);

    let is_online_key = if is_online_val != 0 { 1u16 } else { 0u16 };
    let card_present_key = if card_present_val != 0 { 1u16 } else { 0u16 };
    let unknown_merch = if known { 0u16 } else { 1u16 };
    let sent = if has_last_transaction { 1u16 } else { 0u16 };
    let key = (is_online_key << 12)
        | (card_present_key << 11)
        | (unknown_merch << 10)
        | (sent << 9)
        | (sent << 8)
        | ((requested.hour as u16) << 3)
        | requested.day_of_week as u16;

    Some((out, key))
}

fn find_after(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    haystack
        .get(start..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset + needle.len())
}

fn expect_after(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if haystack.get(start..)?.starts_with(needle) {
        Some(start + needle.len())
    } else {
        None
    }
}

fn find_byte(haystack: &[u8], byte: u8, start: usize) -> Option<usize> {
    haystack
        .get(start..)?
        .iter()
        .position(|&value| value == byte)
        .map(|offset| start + offset)
}

fn parse_f64_at(body: &[u8], start: usize) -> Option<(f64, usize)> {
    let mut end = start;
    while let Some(&byte) = body.get(end) {
        if byte.is_ascii_digit()
            || byte == b'.'
            || byte == b'-'
            || byte == b'+'
            || byte == b'e'
            || byte == b'E'
        {
            end += 1;
        } else {
            break;
        }
    }
    if end == start {
        return None;
    }
    let value = std::str::from_utf8(&body[start..end])
        .ok()?
        .parse::<f64>()
        .ok()?;
    Some((value, end))
}

fn parse_u32_at(body: &[u8], start: usize) -> Option<(u32, usize)> {
    let mut end = start;
    let mut value = 0u32;
    while let Some(&byte) = body.get(end) {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
        end += 1;
    }
    if end == start {
        return None;
    }
    Some((value, end))
}

fn parse_bool_at(body: &[u8], start: usize) -> Option<(bool, usize)> {
    if body.get(start..)?.starts_with(b"true") {
        Some((true, start + 4))
    } else if body.get(start..)?.starts_with(b"false") {
        Some((false, start + 5))
    } else {
        None
    }
}

fn parse_str_at(body: &[u8], start: usize) -> Option<(&str, usize)> {
    let end = find_byte(body, b'"', start)?;
    let value = std::str::from_utf8(&body[start..end]).ok()?;
    Some((value, end + 1))
}

fn known_merchants_contains(mut body: &[u8], merchant_id: &[u8]) -> bool {
    while let Some(start) = body.iter().position(|&byte| byte == b'"') {
        body = &body[start + 1..];
        let Some(end) = body.iter().position(|&byte| byte == b'"') else {
            return false;
        };
        if &body[..end] == merchant_id {
            return true;
        }
        body = &body[end + 1..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Payload;
    use crate::vectorize::vectorize_to_i16_and_key;

    #[test]
    fn fast_payload_matches_vectorize() {
        let body = br#"{"id":"tx-1641912674","transaction":{"amount":441.59,"installments":1,"requested_at":"2027-07-09T16:31:06Z"},"customer":{"avg_amount":883.18,"tx_count_24h":1,"known_merchants":["MERC-004","MERC-017"]},"merchant":{"id":"MERC-004","mcc":"5411","avg_amount":302.78},"terminal":{"is_online":false,"card_present":true,"km_from_home":33.8814492067},"last_transaction":{"timestamp":"2027-06-04T14:14:22Z","km_from_current":18.4353521556}}"#;
        let constants = Constants::load_embedded();
        let payload: Payload<'_> = serde_json::from_slice(body).unwrap();
        assert_eq!(
            parse_payload_to_i16_and_key(body, &constants).unwrap(),
            vectorize_to_i16_and_key(&payload, &constants)
        );
    }
}
