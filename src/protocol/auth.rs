use super::pairing::token_digest;

#[derive(Debug, Default)]
pub struct TokenStore {
    digests: Vec<String>,
}
impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, token: &[u8]) {
        let d = token_digest(token);
        if !self.digests.iter().any(|x| x == &d) {
            self.digests.push(d);
        }
    }
    pub fn revoke(&mut self, token: &[u8]) {
        let d = token_digest(token);
        self.digests.retain(|x| x != &d);
    }
    pub fn contains(&self, token: &[u8]) -> bool {
        let d = token_digest(token);
        self.digests
            .iter()
            .any(|x| constant_time_eq(x.as_bytes(), d.as_bytes()))
    }
    pub fn len(&self) -> usize {
        self.digests.len()
    }
}
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut v = 0u8;
    for (x, y) in a.iter().zip(b) {
        v |= x ^ y;
    }
    v == 0
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn issue_revoke() {
        let mut s = TokenStore::new();
        s.insert(b"a");
        assert!(s.contains(b"a"));
        assert_eq!(s.len(), 1);
        s.revoke(b"a");
        assert!(!s.contains(b"a"));
    }
    #[test]
    fn duplicate_is_idempotent() {
        let mut s = TokenStore::new();
        s.insert(b"a");
        s.insert(b"a");
        assert_eq!(s.len(), 1);
    }
}
