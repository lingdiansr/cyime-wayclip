use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stamp {
    pub device_id: String,
    pub counter: u64,
}

impl Stamp {
    pub fn new(device_id: impl Into<String>, counter: u64) -> Self {
        Self {
            device_id: device_id.into(),
            counter,
        }
    }
    pub fn is_newer_than(&self, previous: Option<&Stamp>) -> bool {
        match previous {
            None => true,
            Some(old) => self.counter > old.counter,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyResult {
    Applied,
    IgnoredStale,
    Duplicate,
}

pub fn compare_stamp(incoming: &Stamp, current: Option<&Stamp>) -> Ordering {
    match current {
        None => Ordering::Greater,
        Some(old) => incoming.counter.cmp(&old.counter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_stamp_applies() {
        assert!(Stamp::new("a", 1).is_newer_than(None));
    }
    #[test]
    fn older_and_equal_do_not_apply() {
        let old = Stamp::new("a", 4);
        assert!(!Stamp::new("b", 3).is_newer_than(Some(&old)));
        assert!(!Stamp::new("b", 4).is_newer_than(Some(&old)));
    }
}
