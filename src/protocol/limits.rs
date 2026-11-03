use super::profile::{validate_text, ProfileError, MAX_TEXT_BYTES};

pub const MAX_JSON_BYTES: usize = 262_144;

pub fn validate_json_body(body: &[u8]) -> Result<(), ProfileError> {
    if body.len() > MAX_JSON_BYTES {
        return Err(ProfileError::TooLarge);
    }
    Ok(())
}

pub fn validate_text_body(body: &[u8]) -> Result<&str, ProfileError> {
    validate_text(body)?;
    Ok(std::str::from_utf8(body).expect("validated UTF-8"))
}

pub fn is_text_size_valid(size: usize) -> bool {
    size <= MAX_TEXT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_limit_is_independent() {
        assert!(validate_json_body(&vec![0; MAX_JSON_BYTES]).is_ok());
        assert!(validate_json_body(&vec![0; MAX_JSON_BYTES + 1]).is_err());
    }
}
