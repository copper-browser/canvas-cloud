//! Credential minting, parsing and hashing (`cvs_` / `cvt_` / `cva_`).

use crate::ids;

pub const SESSION_PREFIX: &str = "cvs_";
pub const TOKEN_PREFIX: &str = "cvt_";
pub const APP_PREFIX: &str = "cva_";

/// Which kind of credential a string is (by prefix).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CredentialKind {
    /// `cvs_` — a login session.
    Session,
    /// `cvt_` — a personal access token.
    Token,
    /// `cva_` — a linked-app key.
    App,
}

impl CredentialKind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Session => SESSION_PREFIX,
            Self::Token => TOKEN_PREFIX,
            Self::App => APP_PREFIX,
        }
    }
}

/// A fresh credential of `kind`: `(plaintext, sha256(plaintext))`. Hand the plaintext to the
/// client once; store only the digest.
pub fn mint(kind: CredentialKind) -> (String, [u8; 32]) {
    let token = format!("{}{}", kind.prefix(), ids::random_token());
    let digest = credential_digest(&token);
    (token, digest)
}

/// What the database stores for a credential: SHA-256 of the whole string, prefix included.
pub fn credential_digest(credential: &str) -> [u8; 32] {
    ids::sha256(credential.as_bytes())
}

/// The kind of a well-formed credential (prefix + exactly 43 base64url characters), or
/// `None` — so malformed input never reaches the database.
pub fn parse_credential(s: &str) -> Option<CredentialKind> {
    let (kind, rest) = if let Some(rest) = s.strip_prefix(SESSION_PREFIX) {
        (CredentialKind::Session, rest)
    } else if let Some(rest) = s.strip_prefix(TOKEN_PREFIX) {
        (CredentialKind::Token, rest)
    } else {
        (CredentialKind::App, s.strip_prefix(APP_PREFIX)?)
    };
    ids::is_token_shape(rest).then_some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_credentials_parse_and_hash() {
        for kind in [
            CredentialKind::Session,
            CredentialKind::Token,
            CredentialKind::App,
        ] {
            let (t, d) = mint(kind);
            assert!(t.starts_with(kind.prefix()));
            assert_eq!(t.len(), 4 + 43);
            assert_eq!(parse_credential(&t), Some(kind));
            assert_eq!(d, credential_digest(&t));
            assert_eq!(d, ids::sha256(t.as_bytes()));
            assert_ne!(mint(kind).0, t);
        }
    }

    #[test]
    fn malformed_credentials_are_rejected() {
        let body = ids::random_token();
        for bad in [
            String::new(),
            body.clone(),
            format!("cvx_{body}"),
            format!("cvs_{}", &body[..42]),
            format!("cvs_{body}A"),
            format!("cvs_{}+", &body[..42]),
            format!("CVS_{body}"),
        ] {
            assert_eq!(parse_credential(&bad), None, "{bad}");
        }
    }
}
