//! NDJSON request/event protocol types for the daemon.
//!
//! One JSON object per line, no trailing newline embedded in the values this
//! module produces (the caller appends `\n` when writing to the stream).

use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Op {
    Fetch {
        uri: String,
        out: String,
    },
    // torrent+data => verified bridge; path => pack. Exactly one form required.
    Convert {
        torrent: Option<String>,
        data: Option<String>,
        path: Option<String>,
    },
    Torrent {
        input: String,
        out: Option<String>,
    },
    Provide {
        nptp: String,
    },
    Unprovide {
        root: String,
    },
    Status {},
    Shutdown {},
}

/// Parses one NDJSON line into a `Request`. The `Err(String)` is meant to be
/// forwarded verbatim as the message of an `error` event.
pub fn parse_request(line: &str) -> Result<Request, String> {
    let req: Request = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if let Op::Convert { torrent, data, path } = &req.op {
        let shape = (torrent.is_some(), data.is_some(), path.is_some());
        match shape {
            (true, true, false) | (false, false, true) => {}
            _ => {
                return Err(
                    "convert requires exactly one of: torrent+data together, or path alone"
                        .to_string(),
                )
            }
        }
    }
    Ok(req)
}

pub fn event_progress(id: u64, op: &str, done: u64, total: u64) -> String {
    json!({
        "id": id,
        "event": "progress",
        "op": op,
        "done": done,
        "total": total,
    })
    .to_string()
}

pub fn event_result(id: u64, fields: serde_json::Value) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_string(), json!(id));
    obj.insert("event".to_string(), json!("result"));
    obj.insert("ok".to_string(), json!(true));
    if let serde_json::Value::Object(map) = fields {
        for (k, v) in map {
            obj.insert(k, v);
        }
    }
    serde_json::Value::Object(obj).to_string()
}

pub fn event_error(id: u64, message: &str) -> String {
    json!({
        "id": id,
        "event": "error",
        "ok": false,
        "message": message,
    })
    .to_string()
}

pub fn event_ready(version: &str) -> String {
    json!({
        "event": "ready",
        "version": version,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fetch() {
        let r = parse_request(r#"{"id":7,"cmd":"fetch","uri":"np2ptp:ab","out":"D:/x"}"#).unwrap();
        assert_eq!(r.id, 7);
        assert!(matches!(r.op, Op::Fetch { .. }));
    }

    #[test]
    fn convert_requires_exactly_one_form() {
        assert!(parse_request(r#"{"id":1,"cmd":"convert","path":"a","torrent":"b","data":"c"}"#)
            .is_err());
        assert!(parse_request(r#"{"id":1,"cmd":"convert"}"#).is_err());
        assert!(
            parse_request(r#"{"id":1,"cmd":"convert","torrent":"t","data":"d"}"#).is_ok()
        );
        assert!(parse_request(r#"{"id":1,"cmd":"convert","path":"p"}"#).is_ok());
    }

    #[test]
    fn convert_rejects_partial_bridge_form() {
        assert!(parse_request(r#"{"id":1,"cmd":"convert","torrent":"t"}"#).is_err());
        assert!(parse_request(r#"{"id":1,"cmd":"convert","data":"d"}"#).is_err());
    }

    #[test]
    fn rejects_unknown_cmd() {
        assert!(parse_request(r#"{"id":1,"cmd":"nope"}"#).is_err());
    }

    #[test]
    fn rejects_missing_id() {
        assert!(parse_request(r#"{"cmd":"status"}"#).is_err());
    }

    #[test]
    fn events_carry_id_and_shape() {
        let line = event_progress(3, "fetch", 10, 100);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["event"], "progress");
        assert_eq!(v["op"], "fetch");
        assert!(!line.ends_with('\n'));
    }

    #[test]
    fn result_event_merges_fields_and_marks_ok() {
        let line = event_result(9, json!({"nptp": "abc"}));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 9);
        assert_eq!(v["event"], "result");
        assert_eq!(v["ok"], true);
        assert_eq!(v["nptp"], "abc");
    }

    #[test]
    fn error_event_shape() {
        let line = event_error(4, "boom");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 4);
        assert_eq!(v["event"], "error");
        assert_eq!(v["ok"], false);
        assert_eq!(v["message"], "boom");
    }

    #[test]
    fn ready_event_shape() {
        let line = event_ready("0.1.8");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "ready");
        assert_eq!(v["version"], "0.1.8");
        assert!(v.get("id").is_none());
    }
}
