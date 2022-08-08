use std::collections::HashMap;
use tokio::time::Duration;
use warp::{http::HeaderMap, Filter};

use crate::{serde::default_decoding, serde::default_framing_message_based};
use codecs::decoding::{
    CharacterDelimitedDecoderOptions, DeserializerConfig, FramingConfig,
    NewlineDelimitedDecoderOptions,
};
use vector_core::event::Event;

use super::HttpScrapeConfig;
use crate::test_util::{
    components::{run_and_assert_source_compliance, HTTP_PULL_SOURCE_TAGS},
    next_addr, test_generate_config, wait_for_tcp,
};

pub(crate) const INTERVAL_SECS: u64 = 1;

/// The happy path should yield at least one event and must emit the required internal events for sources.
pub(crate) async fn run_compliance(config: HttpScrapeConfig) -> Vec<Event> {
    let events =
        run_and_assert_source_compliance(config, Duration::from_secs(3), &HTTP_PULL_SOURCE_TAGS)
            .await;

    assert!(!events.is_empty());

    events
}

#[test]
fn http_scrape_generate_config() {
    test_generate_config::<HttpScrapeConfig>();
}

/// Bytes should be decoded and HTTP header set to text/plain.
#[tokio::test]
async fn bytes_decoding() {
    let in_addr = next_addr();

    // validates the Accept header is set correctly for the Bytes codec
    let dummy_endpoint = warp::path!("endpoint")
        .and(warp::header::exact("Accept", "text/plain"))
        .map(|| r#"A plain text event"#);

    tokio::spawn(warp::serve(dummy_endpoint).run(in_addr));

    run_compliance(HttpScrapeConfig::new(
        format!("http://{}/endpoint", in_addr),
        INTERVAL_SECS,
        None,
        default_decoding(),
        default_framing_message_based(),
        None,
        None,
        None,
    ))
    .await;
}

/// JSON with newline delimiter should be decoded and HTTP header set to application/x-ndjson.
#[tokio::test]
async fn json_decoding_newline_delimited() {
    let in_addr = next_addr();

    // validates the Content-Type is set correctly for the Json codec
    let dummy_endpoint = warp::path!("endpoint")
        .and(warp::header::exact("Accept", "application/x-ndjson"))
        .map(|| r#"{"data" : "foo"}"#);

    tokio::spawn(warp::serve(dummy_endpoint).run(in_addr));
    wait_for_tcp(in_addr).await;

    run_compliance(HttpScrapeConfig::new(
        format!("http://{}/endpoint", in_addr),
        INTERVAL_SECS,
        None,
        DeserializerConfig::Json,
        FramingConfig::NewlineDelimited {
            newline_delimited: NewlineDelimitedDecoderOptions::default(),
        },
        None,
        None,
        None,
    ))
    .await;
}

/// JSON with character delimiter should be decoded and HTTP header set to application/json.
#[tokio::test]
async fn json_decoding_character_delimited() {
    let in_addr = next_addr();

    // validates the Content-Type is set correctly for the Json codec
    let dummy_endpoint = warp::path!("endpoint")
        .and(warp::header::exact("Accept", "application/json"))
        .map(|| r#"{"data" : "foo"}"#);

    tokio::spawn(warp::serve(dummy_endpoint).run(in_addr));
    wait_for_tcp(in_addr).await;

    run_compliance(HttpScrapeConfig::new(
        format!("http://{}/endpoint", in_addr),
        INTERVAL_SECS,
        None,
        DeserializerConfig::Json,
        FramingConfig::CharacterDelimited {
            character_delimited: CharacterDelimitedDecoderOptions {
                delimiter: b',',
                max_length: Some(usize::MAX),
            },
        },
        None,
        None,
        None,
    ))
    .await;
}

/// HTTP request queries configured by the user should be applied correctly.
#[tokio::test]
async fn request_query_applied() {
    let in_addr = next_addr();

    let dummy_endpoint = warp::path!("endpoint")
        .and(warp::query::raw())
        .map(|query| format!(r#"{{"data" : "{}"}}"#, query));

    tokio::spawn(warp::serve(dummy_endpoint).run(in_addr));
    wait_for_tcp(in_addr).await;

    let events = run_compliance(HttpScrapeConfig::new(
        format!("http://{}/endpoint?key1=val1", in_addr),
        INTERVAL_SECS,
        Some(HashMap::from([
            ("key1".to_string(), vec!["val2".to_string()]),
            (
                "key2".to_string(),
                vec!["val1".to_string(), "val2".to_string()],
            ),
        ])),
        DeserializerConfig::Json,
        default_framing_message_based(),
        None,
        None,
        None,
    ))
    .await;

    let logs: Vec<_> = events.into_iter().map(|event| event.into_log()).collect();

    let expected = HashMap::from([
        (
            "key1".to_string(),
            vec!["val1".to_string(), "val2".to_string()],
        ),
        (
            "key2".to_string(),
            vec!["val1".to_string(), "val2".to_string()],
        ),
    ]);

    for log in logs {
        let query = log.get("data").expect("data must be available");
        let mut got: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in
            url::form_urlencoded::parse(query.as_bytes().expect("byte conversion should succeed"))
        {
            got.entry(k.to_string())
                .or_insert_with(Vec::new)
                .push(v.to_string());
        }
        for v in got.values_mut() {
            v.sort();
        }
        assert_eq!(got, expected);
    }
}

/// HTTP request headers configured by the user should be applied correctly.
#[tokio::test]
async fn headers_applied() {
    let in_addr = next_addr();

    let dummy_endpoint = warp::path!("endpoint")
        .and(warp::header::exact("Accept", "text/plain"))
        .and(warp::header::headers_cloned().map(|headers: HeaderMap| {
            let view = headers.get_all("f00");
            let mut iter = view.iter();
            assert_eq!(&"bazz", iter.next().unwrap());
            assert_eq!(&"bizz", iter.next().unwrap());
        }))
        .map(|_| r#"{"data" : "foo"}"#);

    tokio::spawn(warp::serve(dummy_endpoint).run(in_addr));
    wait_for_tcp(in_addr).await;

    run_compliance(HttpScrapeConfig::new(
        format!("http://{}/endpoint", in_addr),
        INTERVAL_SECS,
        None,
        default_decoding(),
        default_framing_message_based(),
        Some(HashMap::from([(
            "f00".to_string(),
            vec!["bazz".to_string(), "bizz".to_string()],
        )])),
        None,
        None,
    ))
    .await;
}
