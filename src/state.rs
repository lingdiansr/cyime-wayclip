use crate::protocol::{compare_stamp, ApplyResult, ClipboardProfile, Stamp};

#[derive(Debug, Default)]
pub struct ClipboardState {
    profile: Option<ClipboardProfile>,
    stamp: Option<Stamp>,
}

impl ClipboardState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn current(&self) -> Option<&ClipboardProfile> {
        self.profile.as_ref()
    }
    pub fn stamp(&self) -> Option<&Stamp> {
        self.stamp.as_ref()
    }
    pub fn apply(&mut self, profile: ClipboardProfile, stamp: Stamp) -> ApplyResult {
        if profile.validate().is_err() {
            return ApplyResult::IgnoredStale;
        }
        match compare_stamp(&stamp, self.stamp.as_ref()) {
            std::cmp::Ordering::Less => ApplyResult::IgnoredStale,
            std::cmp::Ordering::Equal => {
                if self.profile.as_ref() == Some(&profile) {
                    ApplyResult::Duplicate
                } else {
                    ApplyResult::IgnoredStale
                }
            }
            std::cmp::Ordering::Greater => {
                self.profile = Some(profile);
                self.stamp = Some(stamp);
                ApplyResult::Applied
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn applies_and_rejects_stale() {
        let mut s = ClipboardState::new();
        let p = ClipboardProfile::text("a").unwrap();
        assert_eq!(s.apply(p.clone(), Stamp::new("a", 1)), ApplyResult::Applied);
        assert_eq!(
            s.apply(p.clone(), Stamp::new("a", 1)),
            ApplyResult::Duplicate
        );
        assert_eq!(
            s.apply(ClipboardProfile::text("b").unwrap(), Stamp::new("b", 0)),
            ApplyResult::IgnoredStale
        );
        assert_eq!(s.current().unwrap().text, "a");
    }
}
