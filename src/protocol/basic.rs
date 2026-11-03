use base64::{engine::general_purpose::STANDARD, Engine};
pub fn valid_basic(header: &str, code: &str) -> bool {
    let Some(v) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = STANDARD.decode(v) else {
        return false;
    };
    decoded == format!("cyime:{code}").as_bytes()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates() {
        let h = format!("Basic {}", STANDARD.encode("cyime:x"));
        assert!(valid_basic(&h, "x"));
        assert!(!valid_basic(&h, "y"));
    }
}
