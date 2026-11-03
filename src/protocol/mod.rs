mod profile;
pub use profile::{ClipboardProfile, ProfileError};
mod limits;
pub use limits::{is_text_size_valid, validate_json_body, validate_text_body, MAX_JSON_BYTES};
