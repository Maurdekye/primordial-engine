//! G8 (world edits' acceptance): every refusal code and every error code, as an MCP client sees it. Each comes back
//! with `isError: true` and the envelope of api.md 6.3, `code` and `message` at the top, and each is named in api.md.
//! The lists are exhaustive matches, so a code added to S6 doesn't build here until it is listed.

use pe_mcp::server::{api_err, refused};
use pe_seams::world_api::{ApiError, ErrorCode, Outcome, Refusal, RefusalCode, Status};
use serde_json::Value;

const SPEC: &str = include_str!("../../../docs/spec/api.md");

/// Every refusal code, once. The match makes a new variant a compile error until it is listed.
fn refusal_codes() -> Vec<RefusalCode> {
    use RefusalCode as C;
    let all = vec![
        C::StaleVersion,
        C::NotFound,
        C::Ambiguous,
        C::Invalid,
        C::InChange,
        C::TooMany,
        C::NotPermitted,
        C::Protected,
        C::Held,
        C::TooLarge,
        C::RateLimited,
        C::NotReady,
        C::Compile,
        C::Dropped,
    ];
    for c in &all {
        match c {
            C::StaleVersion
            | C::NotFound
            | C::Ambiguous
            | C::Invalid
            | C::InChange
            | C::TooMany
            | C::NotPermitted
            | C::Protected
            | C::Held
            | C::TooLarge
            | C::RateLimited
            | C::NotReady
            | C::Compile
            | C::Dropped => {}
        }
    }
    all
}

/// Every error code, once, likewise.
fn error_codes() -> Vec<ErrorCode> {
    use ErrorCode as E;
    let all = vec![
        E::InvalidArgument,
        E::Forbidden,
        E::CursorExpired,
        E::SnapshotGone,
        E::CaptureUnavailable,
        E::RateLimited,
        E::Timeout,
        E::SessionEnded,
        E::Unavailable,
        E::Internal,
    ];
    for c in &all {
        match c {
            E::InvalidArgument
            | E::Forbidden
            | E::CursorExpired
            | E::SnapshotGone
            | E::CaptureUnavailable
            | E::RateLimited
            | E::Timeout
            | E::SessionEnded
            | E::Unavailable
            | E::Internal => {}
        }
    }
    all
}

fn name<T: serde::Serialize>(code: &T) -> String {
    serde_json::to_value(code).expect("a code").as_str().expect("a string").to_string()
}

fn outcome(status: Status, refusal: Option<Refusal>) -> Outcome {
    Outcome {
        dry_run: false,
        status,
        repaired_at: None,
        is_final: false,
        change: None,
        frame: None,
        final_frame: 7,
        created: Default::default(),
        changed: vec![],
        count: 0,
        refusal,
        diff: None,
        resent_as: None,
        held: None,
        activation: None,
        scripts: Default::default(),
    }
}

fn envelope(r: &rmcp::model::CallToolResult) -> Value {
    assert_eq!(r.is_error, Some(true), "{r:?}");
    r.structured_content.clone().expect("the envelope as structured content")
}

#[test]
fn every_refusal_code_reaches_the_client_as_an_error_with_its_envelope() {
    for code in refusal_codes() {
        let f = Refusal {
            code,
            message: format!("Refused as {code:?}."),
            op: Some(2),
            object: None,
            field: Some("Transform.position".into()),
            read: None,
            now: None,
            winner: None,
            next: Some("Do this next.".into()),
            details: Some(serde_json::json!({ "limit": 3 })),
        };
        let v = envelope(&refused(&outcome(Status::Refused, Some(f))));
        let n = name(&code);
        assert_eq!(v["code"], n.as_str(), "{v}");
        assert_eq!(v["message"], format!("Refused as {code:?}."), "{v}");
        // The rest of the refusal and of the outcome, in details.
        let d = &v["details"];
        assert_eq!((d["op"].as_u64(), d["field"].as_str()), (Some(2), Some("Transform.position")), "{v}");
        assert_eq!((d["next"].as_str(), d["details"]["limit"].as_u64()), (Some("Do this next."), Some(3)), "{v}");
        assert_eq!(d["final_frame"], 7, "{v}");
        assert!(d.get("code").is_none() && d.get("message").is_none(), "only at the top: {v}");
        assert!(SPEC.contains(&format!("`{n}`")), "api.md names the refusal code `{n}`");
    }
    // A dropped change carries no refusal: `dropped`, with a message of its own.
    let v = envelope(&refused(&outcome(Status::Dropped, None)));
    assert_eq!(v["code"], "dropped");
    assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()), "{v}");
}

#[test]
fn every_error_code_reaches_the_client_as_an_error_with_its_envelope() {
    for code in error_codes() {
        let mut e = ApiError::new(code, format!("Failed as {code:?}."));
        e.field = Some("limit".into());
        let v = envelope(&api_err(e));
        let n = name(&code);
        assert_eq!((v["code"].as_str(), v["field"].as_str()), (Some(n.as_str()), Some("limit")), "{v}");
        assert_eq!(v["message"], format!("Failed as {code:?}."), "{v}");
        assert!(SPEC.contains(&format!("`{n}`")), "api.md names the error code `{n}`");
    }
}
