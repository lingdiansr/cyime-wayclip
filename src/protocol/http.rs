use super::{ClipboardProfile, MAX_JSON_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpStatus {
    Ok,
    NoContent,
    NotModified,
    Unauthorized,
    Forbidden,
    BadRequest,
    PayloadTooLarge,
    UnsupportedMediaType,
    ServiceUnavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum GetResult {
    Profile(ClipboardProfile),
    NotModified,
    NoContent,
    Forbidden,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PutResult {
    Accepted(ClipboardProfile),
    Unauthorized,
    Forbidden,
    BadRequest,
    PayloadTooLarge,
    UnsupportedMediaType,
    Unavailable,
}

pub fn get(
    profile: Option<&ClipboardProfile>,
    if_none_match: Option<&str>,
    send_enabled: bool,
    available: bool,
) -> GetResult {
    if !available {
        return GetResult::Unavailable;
    }
    if !send_enabled {
        return GetResult::Forbidden;
    }
    let Some(profile) = profile else {
        return GetResult::NoContent;
    };
    if if_none_match == Some(profile.hash.as_str()) {
        GetResult::NotModified
    } else {
        GetResult::Profile(profile.clone())
    }
}

pub fn put(body: &[u8], receive_enabled: bool, authenticated: bool, available: bool) -> PutResult {
    if !authenticated {
        return PutResult::Unauthorized;
    }
    if !receive_enabled {
        return PutResult::Forbidden;
    }
    if !available {
        return PutResult::Unavailable;
    }
    if body.len() > MAX_JSON_BYTES {
        return PutResult::PayloadTooLarge;
    }
    let Ok(profile) = serde_json::from_slice::<ClipboardProfile>(body) else {
        return PutResult::BadRequest;
    };
    if profile.validate().is_err() {
        return PutResult::BadRequest;
    }
    PutResult::Accepted(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p() -> ClipboardProfile {
        ClipboardProfile::text("hello").unwrap()
    }
    #[test]
    fn get_contract() {
        let x = p();
        assert!(matches!(
            get(Some(&x), None, true, true),
            GetResult::Profile(_)
        ));
        assert_eq!(
            get(Some(&x), Some(&x.hash), true, true),
            GetResult::NotModified
        );
        assert_eq!(get(None, None, true, true), GetResult::NoContent);
    }
    #[test]
    fn put_contract() {
        let x = p();
        let b = serde_json::to_vec(&x).unwrap();
        assert_eq!(put(&b, true, true, true), PutResult::Accepted(x));
        assert_eq!(put(&b, true, false, true), PutResult::Unauthorized);
        assert_eq!(put(&b, false, true, true), PutResult::Forbidden);
        assert_eq!(put(&[1], true, true, true), PutResult::BadRequest);
    }
    #[test]
    fn limits() {
        assert_eq!(
            put(&vec![0; MAX_JSON_BYTES + 1], true, true, true),
            PutResult::PayloadTooLarge
        );
    }
}
