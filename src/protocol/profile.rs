use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const MAX_TEXT_BYTES: usize = 65_536;
pub const MAX_SOURCE_BYTES: usize = 128;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("text exceeds maximum size of {MAX_TEXT_BYTES} bytes")]
    TooLarge,
    #[error("text contains a NUL byte")]
    Nul,
    #[error("text must be valid UTF-8")]
    InvalidUtf8,
    #[error("source exceeds maximum size of {MAX_SOURCE_BYTES} bytes")]
    SourceTooLarge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardProfile {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    pub hash: String,
    pub size: usize,
    pub has_data: bool,
    pub data_name: Option<String>,
    pub source: String,
}

impl ClipboardProfile {
    pub fn text(text: impl Into<String>) -> Result<Self, ProfileError> {
        let text = text.into();
        validate_text(text.as_bytes())?;
        Ok(Self {
            hash: hash_text(text.as_bytes()),
            size: text.len(),
            kind: "text".into(),
            text,
            has_data: false,
            data_name: None,
            source: "CyIME-Wayclip".into(),
        })
    }

    pub fn validate(&self) -> Result<(), ProfileError> {
        validate_text(self.text.as_bytes())?;
        if self.source.len() > MAX_SOURCE_BYTES {
            return Err(ProfileError::SourceTooLarge);
        }
        Ok(())
    }
}

pub fn validate_text(bytes: &[u8]) -> Result<(), ProfileError> {
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(ProfileError::TooLarge);
    }
    if bytes.contains(&0) {
        return Err(ProfileError::Nul);
    }
    std::str::from_utf8(bytes)
        .map(|_| ())
        .map_err(|_| ProfileError::InvalidUtf8)
}

pub fn hash_text(text: &[u8]) -> String {
    let digest = Sha256::digest(text);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_unicode_and_crlf() {
        let p = ClipboardProfile::text("你好\r\n🌍").unwrap();
        assert_eq!(p.size, "你好\r\n🌍".len());
        assert_eq!(p.hash, hash_text(p.text.as_bytes()));
    }
    #[test]
    fn enforces_boundaries() {
        assert!(ClipboardProfile::text("a".repeat(MAX_TEXT_BYTES)).is_ok());
        assert_eq!(
            ClipboardProfile::text("a".repeat(MAX_TEXT_BYTES + 1)).unwrap_err(),
            ProfileError::TooLarge
        );
    }
    #[test]
    fn rejects_nul() {
        assert_eq!(
            ClipboardProfile::text("a\0b").unwrap_err(),
            ProfileError::Nul
        );
    }
    #[test]
    fn json_shape_is_stable() {
        let p = ClipboardProfile::text("hello").unwrap();
        let value: serde_json::Value = serde_json::to_value(p).unwrap();
        assert_eq!(value["type"], "text");
        assert_eq!(value["size"], 5);
    }
}
