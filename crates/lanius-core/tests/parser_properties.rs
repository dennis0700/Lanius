use lanius_core::upstream::{AwsEventStreamParser, ParserEvent};
use proptest::prelude::*;

fn feed_in_chunks(payload: &[u8], sizes: &[usize]) -> Vec<ParserEvent> {
    let mut parser = AwsEventStreamParser::new();
    let mut events = Vec::new();
    let mut offset = 0usize;

    for &size in sizes {
        if offset >= payload.len() {
            break;
        }
        let end = (offset + size.max(1)).min(payload.len());
        events.extend(parser.feed(&payload[offset..end]));
        offset = end;
    }
    if offset < payload.len() {
        events.extend(parser.feed(&payload[offset..]));
    }
    events
}

fn build_payload(contents: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in contents {
        out.extend_from_slice(b"\x00\x00\x00\x8a:message-type\x07\x00\x05event");
        let obj = serde_json::json!({ "content": c });
        out.extend_from_slice(serde_json::to_string(&obj).unwrap().as_bytes());
    }
    out
}

proptest! {
    #[test]
    fn parse_is_independent_of_chunk_boundaries(
        contents in prop::collection::vec("[a-zA-Z0-9 ,.!?_-]{1,40}", 1..12),
        split_sizes in prop::collection::vec(1usize..64, 1..40),
    ) {
        let payload = build_payload(&contents);

        let whole = feed_in_chunks(&payload, &[payload.len().max(1)]);
        let split = feed_in_chunks(&payload, &split_sizes);

        prop_assert_eq!(&whole, &split);
    }

    #[test]
    fn byte_at_a_time_matches_whole(
        contents in prop::collection::vec("[a-zA-Z0-9 ]{1,30}", 1..8),
    ) {
        let payload = build_payload(&contents);

        let whole = feed_in_chunks(&payload, &[payload.len().max(1)]);
        let per_byte = feed_in_chunks(&payload, &vec![1usize; payload.len()]);

        prop_assert_eq!(&whole, &per_byte);
    }

    #[test]
    fn multibyte_content_survives_arbitrary_splits(
        contents in prop::collection::vec("[\u{4e00}-\u{9fa5}]{1,20}", 1..6),
        split_sizes in prop::collection::vec(1usize..12, 1..60),
    ) {
        let payload = build_payload(&contents);

        let whole = feed_in_chunks(&payload, &[payload.len().max(1)]);
        let split = feed_in_chunks(&payload, &split_sizes);

        prop_assert_eq!(&whole, &split);

        let got: Vec<String> = split
            .iter()
            .filter_map(|e| match e {
                ParserEvent::Content(serde_json::Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let mut expected: Vec<String> = Vec::new();
        let mut last: Option<&String> = None;
        for c in &contents {
            if last != Some(c) {
                expected.push(c.clone());
            }
            last = Some(c);
        }
        prop_assert_eq!(got, expected);
    }

    #[test]
    fn tool_arguments_are_split_independent(
        keys in prop::collection::vec("[a-z]{1,8}", 1..5),
        split_sizes in prop::collection::vec(1usize..16, 1..50),
    ) {
        let mut obj = serde_json::Map::new();
        for (i, k) in keys.iter().enumerate() {
            obj.insert(k.clone(), serde_json::json!(i));
        }
        let args_json = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap();

        let mut payload = Vec::new();
        payload.extend_from_slice(br#"{"name":"tool","toolUseId":"call_fixed","input":{}}"#);
        let chars: Vec<char> = args_json.chars().collect();
        for part in chars.chunks(chars.len().div_ceil(3)) {
            let frag: String = part.iter().collect();
            let ev = serde_json::json!({ "input": frag });
            payload.extend_from_slice(serde_json::to_string(&ev).unwrap().as_bytes());
        }
        payload.extend_from_slice(br#"{"stop":true}"#);

        let collect_args = |sizes: &[usize]| -> Vec<String> {
            let mut parser = AwsEventStreamParser::new();
            let mut offset = 0usize;
            for &size in sizes {
                if offset >= payload.len() { break; }
                let end = (offset + size.max(1)).min(payload.len());
                parser.feed(&payload[offset..end]);
                offset = end;
            }
            if offset < payload.len() {
                parser.feed(&payload[offset..]);
            }
            parser.take_tool_calls().into_iter().map(|t| t.arguments).collect()
        };

        let whole = collect_args(&[payload.len().max(1)]);
        let split = collect_args(&split_sizes);

        prop_assert_eq!(&whole, &split);
        prop_assert_eq!(whole.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&whole[0])
            .map_err(|e| TestCaseError::fail(format!("invalid JSON {:?}: {e}", whole[0])))?;
        for k in &keys {
            prop_assert!(parsed.get(k).is_some(), "missing key {k} in {:?}", whole[0]);
        }
    }
}
