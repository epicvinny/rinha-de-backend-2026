pub const BAD_REQUEST_RESPONSE: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const NOT_FOUND_RESPONSE: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const METHOD_NOT_ALLOWED_RESPONSE: &[u8] =
    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const PAYLOAD_TOO_LARGE_RESPONSE: &[u8] =
    b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const SERVICE_UNAVAILABLE_RESPONSE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const READY_OK_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";

const SCORE_0_HTTP_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0}";
const SCORE_1_HTTP_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}";
const SCORE_2_HTTP_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}";
const SCORE_3_HTTP_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}";
const SCORE_4_HTTP_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}";
const SCORE_5_HTTP_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 34\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":1}";

#[inline]
pub fn bucket_for_fraud_score(fraud_score: f64) -> u8 {
    (fraud_score * 5.0 + 0.1) as u8
}

#[inline]
pub fn body_for_bucket(bucket: u8) -> &'static str {
    match bucket {
        0 => r#"{"approved":true,"fraud_score":0}"#,
        1 => r#"{"approved":true,"fraud_score":0.2}"#,
        2 => r#"{"approved":true,"fraud_score":0.4}"#,
        3 => r#"{"approved":false,"fraud_score":0.6}"#,
        4 => r#"{"approved":false,"fraud_score":0.8}"#,
        5 => r#"{"approved":false,"fraud_score":1}"#,
        _ => r#"{"approved":false,"fraud_score":1}"#,
    }
}

#[inline]
pub fn http_response_for_bucket(bucket: u8) -> Option<&'static [u8]> {
    match bucket {
        0 => Some(SCORE_0_HTTP_RESPONSE),
        1 => Some(SCORE_1_HTTP_RESPONSE),
        2 => Some(SCORE_2_HTTP_RESPONSE),
        3 => Some(SCORE_3_HTTP_RESPONSE),
        4 => Some(SCORE_4_HTTP_RESPONSE),
        5 => Some(SCORE_5_HTTP_RESPONSE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_http_responses_have_matching_content_length() {
        for bucket in 0..=5 {
            let response = http_response_for_bucket(bucket).unwrap();
            let header_end = response
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("header end")
                + 4;
            let header = std::str::from_utf8(&response[..header_end]).unwrap();
            let content_length = header
                .split("\r\n")
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert_eq!(content_length, response.len() - header_end);
        }
    }
}
