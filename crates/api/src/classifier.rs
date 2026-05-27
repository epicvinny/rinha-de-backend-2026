use crate::tree_model;
use shared::vectorize::parse_iso8601;

#[derive(Clone, Copy, Debug, PartialEq)]
struct FastFields<'a> {
    amount: f32,
    installments: f32,
    requested_at: &'a [u8],
    customer_avg: f32,
    tx_count_24h: f32,
    known_merchants: &'a [u8],
    merchant_id: &'a [u8],
    merchant_mcc: &'a [u8],
    merchant_avg: f32,
    is_online: bool,
    card_present: bool,
    km_from_home: f32,
    last_timestamp: Option<&'a [u8]>,
    km_from_current: f32,
}

#[inline]
pub fn classify_approved(body: &[u8]) -> Option<bool> {
    score_tree_body(body)
}

#[inline]
fn score_tree_body(body: &[u8]) -> Option<bool> {
    let fields = parse_fast_fields(body)?;
    let requested = parse_iso_bytes(fields.requested_at)?;

    let mut minutes_since_last = -1.0f32;
    let mut km_from_last = -1.0f32;
    let mut last_null = 1.0f32;

    if let Some(last_timestamp) = fields.last_timestamp {
        let last = parse_iso_bytes(last_timestamp)?;
        let delta_seconds = requested.epoch_seconds - last.epoch_seconds;
        let positive_delta = if delta_seconds > 0 { delta_seconds } else { 0 };
        minutes_since_last = clamp01(positive_delta as f32 / 60.0 / 1440.0);
        km_from_last = clamp01(fields.km_from_current / 1000.0);
        last_null = 0.0;
    }

    let safe_avg = if fields.customer_avg <= 0.0 {
        1.0
    } else {
        fields.customer_avg
    };
    let amount_ratio = fields.amount / safe_avg;
    let merchant_known = known_merchant_in_array(fields.known_merchants, fields.merchant_id);

    if fields.amount <= 500.0
        && amount_ratio <= 0.50001
        && fields.installments <= 3.0
        && fields.tx_count_24h <= 5.0
        && merchant_known
        && fields.km_from_home <= 50.0
        && is_safe_mcc(fields.merchant_mcc)
    {
        return Some(true);
    }

    if fields.amount >= 5000.0
        && fields.installments >= 5.0
        && fields.tx_count_24h >= 6.0
        && !merchant_known
        && fields.km_from_home >= 150.0
        && is_risky_mcc(fields.merchant_mcc)
    {
        return Some(false);
    }

    let features = [
        clamp01(fields.amount / 10000.0),
        clamp01(fields.installments / 12.0),
        clamp01(amount_ratio / 10.0),
        requested.hour as f32 / 23.0,
        requested.day_of_week as f32 / 6.0,
        minutes_since_last,
        km_from_last,
        clamp01(fields.km_from_home / 1000.0),
        clamp01(fields.tx_count_24h / 20.0),
        if fields.is_online { 1.0 } else { 0.0 },
        if fields.card_present { 1.0 } else { 0.0 },
        if merchant_known { 0.0 } else { 1.0 },
        mcc_risk(fields.merchant_mcc),
        clamp01(fields.merchant_avg / 10000.0),
        last_null,
        fields.amount,
        fields.customer_avg,
        amount_ratio,
        fields.tx_count_24h,
        fields.km_from_home,
        fields.merchant_avg,
    ];

    Some(!tree_model::predict(&features))
}

fn parse_fast_fields(body: &[u8]) -> Option<FastFields<'_>> {
    let mut pos = 0usize;

    pos = find_after(body, b"\"transaction\":{\"amount\":", pos)?;
    let (amount, next) = parse_number_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"installments\":", pos)?;
    let (installments, next) = parse_number_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"requested_at\":", pos)?;
    let (requested_at, next) = parse_string_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b"},\"customer\":{\"avg_amount\":", pos)?;
    let (customer_avg, next) = parse_number_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"tx_count_24h\":", pos)?;
    let (tx_count_24h, next) = parse_number_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"known_merchants\":", pos)?;
    let (known_merchants, next) = parse_array_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b"},\"merchant\":{\"id\":", pos)?;
    let (merchant_id, next) = parse_string_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"mcc\":", pos)?;
    let (merchant_mcc, next) = parse_string_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"avg_amount\":", pos)?;
    let (merchant_avg, next) = parse_number_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b"},\"terminal\":{\"is_online\":", pos)?;
    let (is_online, next) = parse_bool_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"card_present\":", pos)?;
    let (card_present, next) = parse_bool_at(body, pos)?;
    pos = next;

    pos = expect_after(body, b",\"km_from_home\":", pos)?;
    let (km_from_home, next) = parse_number_at(body, pos)?;
    pos = next;

    let last_value_pos = expect_after(body, b"},\"last_transaction\":", pos)?;
    let mut last_timestamp = None;
    let mut km_from_current = -1.0f32;
    if !body.get(last_value_pos..)?.starts_with(b"null") {
        pos = last_value_pos;
        pos = expect_after(body, b"{\"timestamp\":", pos)?;
        let (timestamp, next) = parse_string_at(body, pos)?;
        last_timestamp = Some(timestamp);
        pos = next;

        pos = expect_after(body, b",\"km_from_current\":", pos)?;
        let (value, _) = parse_number_at(body, pos)?;
        km_from_current = value;
    }

    Some(FastFields {
        amount,
        installments,
        requested_at,
        customer_avg,
        tx_count_24h,
        known_merchants,
        merchant_id,
        merchant_mcc,
        merchant_avg,
        is_online,
        card_present,
        km_from_home,
        last_timestamp,
        km_from_current,
    })
}

fn parse_string_at(body: &[u8], value_pos: usize) -> Option<(&[u8], usize)> {
    let mut i = value_pos;
    if *body.get(i)? != b'"' {
        return None;
    }
    i += 1;
    let start = i;
    while i < body.len() {
        if body[i] == b'"' && (i == start || body[i - 1] != b'\\') {
            return Some((&body[start..i], i + 1));
        }
        i += 1;
    }
    None
}

fn parse_array_at(body: &[u8], value_pos: usize) -> Option<(&[u8], usize)> {
    let mut i = value_pos;
    if *body.get(i)? != b'[' {
        return None;
    }
    i += 1;
    let start = i;
    while i < body.len() {
        if body[i] == b']' {
            return Some((&body[start..i], i + 1));
        }
        i += 1;
    }
    None
}

#[inline]
fn parse_bool_at(body: &[u8], value_pos: usize) -> Option<(bool, usize)> {
    if body.get(value_pos..)?.starts_with(b"true") {
        Some((true, value_pos + 4))
    } else if body.get(value_pos..)?.starts_with(b"false") {
        Some((false, value_pos + 5))
    } else {
        None
    }
}

fn parse_number_at(body: &[u8], value_pos: usize) -> Option<(f32, usize)> {
    let mut i = value_pos;
    let start = i;

    let mut negative = false;
    if i < body.len() && (body[i] == b'-' || body[i] == b'+') {
        negative = body[i] == b'-';
        i += 1;
    }

    let mut value = 0.0f32;
    let mut digits = 0usize;
    while i < body.len() && body[i].is_ascii_digit() {
        value = value * 10.0 + (body[i] - b'0') as f32;
        digits += 1;
        i += 1;
    }

    if i < body.len() && body[i] == b'.' {
        i += 1;
        let mut scale = 0.1f32;
        while i < body.len() && body[i].is_ascii_digit() {
            value += (body[i] - b'0') as f32 * scale;
            scale *= 0.1;
            digits += 1;
            i += 1;
        }
    }

    if digits == 0 {
        return None;
    }
    if i < body.len() && (body[i] == b'e' || body[i] == b'E') {
        i += 1;
        if i < body.len() && (body[i] == b'-' || body[i] == b'+') {
            i += 1;
        }
        while i < body.len() && body[i].is_ascii_digit() {
            i += 1;
        }
        let value = std::str::from_utf8(&body[start..i])
            .ok()?
            .parse::<f32>()
            .ok()?;
        return Some((value, i));
    }

    Some((if negative { -value } else { value }, i))
}

fn known_merchant_in_array(mut array: &[u8], merchant_id: &[u8]) -> bool {
    while let Some(start) = array.iter().position(|&byte| byte == b'"') {
        array = &array[start + 1..];
        let Some(end) = array.iter().position(|&byte| byte == b'"') else {
            return false;
        };
        if &array[..end] == merchant_id {
            return true;
        }
        array = &array[end + 1..];
    }
    false
}

#[inline]
fn mcc_risk(mcc: &[u8]) -> f32 {
    match mcc {
        b"5411" => 0.15,
        b"5812" => 0.30,
        b"5912" => 0.20,
        b"5944" => 0.45,
        b"7801" => 0.80,
        b"7802" => 0.75,
        b"7995" => 0.85,
        b"4511" => 0.35,
        b"5311" => 0.25,
        b"5999" => 0.50,
        _ => 0.50,
    }
}

#[inline]
fn is_safe_mcc(mcc: &[u8]) -> bool {
    matches!(mcc, b"5411" | b"5812" | b"5912" | b"5311")
}

#[inline]
fn is_risky_mcc(mcc: &[u8]) -> bool {
    matches!(mcc, b"7995" | b"7801" | b"7802")
}

#[inline]
fn parse_iso_bytes(bytes: &[u8]) -> Option<shared::vectorize::FastDateTime> {
    parse_iso8601(std::str::from_utf8(bytes).ok()?)
}

#[inline]
fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_instantiates_tree_predictor() {
        let body = br#"{"transaction":{"amount":41.12,"installments":2,"requested_at":"2026-03-11T18:45:53Z"},"customer":{"avg_amount":82.24,"tx_count_24h":3,"known_merchants":["MERC-003","MERC-016"]},"merchant":{"id":"MERC-016","mcc":"5411","avg_amount":60.25},"terminal":{"is_online":false,"card_present":true,"km_from_home":29.23},"last_transaction":null}"#;
        assert!(classify_approved(body).is_some());
    }
}
