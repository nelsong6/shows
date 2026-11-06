//! Checks the token another PC's remote presents on the player's network port.
//!
//! OpenBao signs the token (nelsong6/openbao, role `shows`) for a device that logged in
//! with its own key. The player holds no secret: it checks the signature against the
//! keys OpenBao publishes, fetched when a token names a key it hasn't seen.

use std::sync::Mutex;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde_json::Value;

pub const ISSUER: &str = "https://bao.home.romaine.life/v1/identity/oidc";
const KEYS_URL: &str = "https://bao.home.romaine.life/v1/identity/oidc/.well-known/keys";
const AUDIENCE: &str = "shows";

type FetchKeys = Box<dyn Fn() -> Result<JwkSet, String> + Send + Sync>;

pub struct TokenChecker {
    fetch: FetchKeys,
    keys: Mutex<JwkSet>,
}

fn openbao_keys() -> Result<JwkSet, String> {
    let body = ureq::get(KEYS_URL)
        .call()
        .and_then(|mut response| response.body_mut().read_to_string())
        .map_err(|e| format!("cannot read OpenBao's keys: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("invalid OpenBao keys: {e}"))
}

impl TokenChecker {
    pub fn openbao() -> Self {
        Self::with_fetch(Box::new(openbao_keys))
    }

    pub fn with_fetch(fetch: FetchKeys) -> Self {
        Self {
            fetch,
            keys: Mutex::new(JwkSet { keys: Vec::new() }),
        }
    }

    fn key(&self, kid: &str) -> Result<DecodingKey, String> {
        let mut keys = self.keys.lock().unwrap();
        if keys.find(kid).is_none() {
            *keys = (self.fetch)()?;
        }
        let jwk = keys
            .find(kid)
            .ok_or("the token's signing key isn't one of OpenBao's")?;
        DecodingKey::from_jwk(jwk).map_err(|e| format!("invalid OpenBao key: {e}"))
    }

    pub fn check(&self, token: &str) -> Result<(), String> {
        let header = decode_header(token).map_err(|_| "not a signed token")?;
        let kid = header.kid.ok_or("the token names no signing key")?;
        let key = self.key(&kid)?;
        let mut validation = Validation::new(Algorithm::ES256);
        validation.set_audience(&[AUDIENCE]);
        validation.set_issuer(&[ISSUER]);
        decode::<Value>(token, &key, &validation)
            .map(|_| ())
            .map_err(|e| format!("token refused: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A throwaway P-256 pair for these tests, generated for this file.
    const KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgKL/xHHDaM1YrKnv2
mDuMrlN4kCyK6H6Zctd4R+n5AAmhRANCAAS5N+qDIf54e0j5JWN5q2/oG8JJm9YN
RXcElwx4NzzIjreNOTuPo860Vm9cKijgXPCxamwY+f4Lt32uD/NqurAW
-----END PRIVATE KEY-----
";
    const X: &str = "uTfqgyH-eHtI-SVjeatv6BvCSZvWDUV3BJcMeDc8yI4";
    const Y: &str = "t405O4-jzrRWb1wqKOBc8LFqbBj5_gu3fa4P82q6sBY";

    fn keys(kid: &str) -> JwkSet {
        serde_json::from_value(json!({"keys":[{"kty":"EC","crv":"P-256","use":"sig","alg":"ES256","kid":kid,"x":X,"y":Y}]})).unwrap()
    }

    fn token(kid: &str, claims: Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(kid.into());
        encode(&header, &claims, &EncodingKey::from_ec_pem(KEY.as_bytes()).unwrap()).unwrap()
    }

    fn claims(aud: &str, iss: &str, left: i64) -> Value {
        json!({"aud": aud, "iss": iss, "exp": chrono::Utc::now().timestamp() + left})
    }

    fn checker(kid: &'static str) -> (TokenChecker, Arc<AtomicUsize>) {
        let fetches = Arc::new(AtomicUsize::new(0));
        let count = fetches.clone();
        (
            TokenChecker::with_fetch(Box::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(keys(kid))
            })),
            fetches,
        )
    }

    #[test]
    fn a_shows_token_from_openbao_passes_and_keys_are_fetched_only_when_new() {
        let (checker, fetches) = checker("k1");
        for _ in 0..3 {
            checker.check(&token("k1", claims("shows", ISSUER, 300))).unwrap();
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn another_audience_issuer_key_or_an_expired_token_is_refused() {
        let (checker, _) = checker("k1");
        assert!(checker.check(&token("k1", claims("openproject", ISSUER, 300))).is_err());
        assert!(checker.check(&token("k1", claims("shows", "https://elsewhere", 300))).is_err());
        assert!(checker.check(&token("k1", claims("shows", ISSUER, -600))).is_err());
        assert!(checker.check(&token("unknown", claims("shows", ISSUER, 300))).is_err());
        assert!(checker.check("not a token").is_err());
    }

    #[test]
    fn a_token_signed_by_another_key_with_a_known_kid_is_refused() {
        let (checker, _) = checker("k1");
        let good = token("k1", claims("shows", ISSUER, 300));
        let (head, rest) = good.split_once('.').unwrap();
        let (_, sig) = rest.rsplit_once('.').unwrap();
        let forged = serde_json::to_vec(&json!({"aud":"shows","iss":ISSUER,"exp":4102444800u64})).unwrap();
        use base64::Engine;
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(forged);
        assert!(checker.check(&format!("{head}.{body}.{sig}")).is_err());
    }

    #[test]
    fn an_unreachable_key_source_refuses_rather_than_passes() {
        let checker = TokenChecker::with_fetch(Box::new(|| Err("OpenBao is down".into())));
        assert_eq!(checker.check(&token("k1", claims("shows", ISSUER, 300))).unwrap_err(), "OpenBao is down");
    }
}
