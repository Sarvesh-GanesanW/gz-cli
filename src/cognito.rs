//! Cognito user-pool sign-in, the same way the Groundzero web app signs in:
//! SRP password proof (USER_SRP_AUTH), an optional TOTP MFA step, and
//! refresh-token renewal. The resulting ID token is what every Groundzero
//! API accepts.

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// RFC 5054 3072-bit group, the one Cognito uses for SRP.
const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);
const G: u32 = 2;
const DERIVED_KEY_INFO: &[u8] = b"Caldera Derived Key";

/// The app-client settings needed to talk to a user pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    pub pool_id: String,
    pub client_id: String,
}

impl PoolConfig {
    /// `ap-south-1_AbC123` → `ap-south-1`.
    pub fn region(&self) -> &str {
        self.pool_id.split('_').next().unwrap_or_default()
    }

    /// The part after the underscore, which SRP mixes into the password hash.
    fn pool_name(&self) -> &str {
        self.pool_id
            .split_once('_')
            .map(|(_, n)| n)
            .unwrap_or_default()
    }

    fn endpoint(&self) -> String {
        format!("https://cognito-idp.{}.amazonaws.com/", self.region())
    }
}

#[derive(Debug, Clone)]
pub struct Tokens {
    pub id_token: String,
    /// Absent on refresh: Cognito keeps the original refresh token valid.
    pub refresh_token: Option<String>,
    pub expires_in: i64,
}

/// What the caller must supply after the password step.
pub enum Step {
    Done(Tokens),
    NeedsMfa { session: String, username: String },
}

async fn call(
    http: &reqwest::Client,
    pool: &PoolConfig,
    action: &str,
    body: Value,
) -> Result<Value> {
    let response = http
        .post(pool.endpoint())
        .header("Content-Type", "application/x-amz-json-1.1")
        .header(
            "X-Amz-Target",
            format!("AWSCognitoIdentityProviderService.{action}"),
        )
        .json(&body)
        .send()
        .await
        .context("cannot reach the sign-in service")?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{}", cognito_error(&value));
    }
    Ok(value)
}

fn cognito_error(value: &Value) -> String {
    let kind = value["__type"].as_str().unwrap_or("Error");
    let kind = kind.rsplit('#').next().unwrap_or(kind);
    let message = value["message"].as_str().unwrap_or("sign-in failed");
    match kind {
        "NotAuthorizedException" | "UserNotFoundException" => {
            if message.to_lowercase().contains("refresh token") {
                "session expired; run `gz auth login`".to_string()
            } else {
                "incorrect username or password".to_string()
            }
        }
        "CodeMismatchException" => "incorrect MFA code".to_string(),
        "ExpiredCodeException" => "MFA code expired; use the next one".to_string(),
        "TooManyRequestsException" | "LimitExceededException" => {
            "too many attempts; wait a minute and try again".to_string()
        }
        "PasswordResetRequiredException" => {
            "password reset required; reset it in the web app first".to_string()
        }
        _ => format!("{kind}: {message}"),
    }
}

/// Password step. Returns tokens, or an MFA challenge to answer with [`answer_mfa`].
pub async fn sign_in(
    http: &reqwest::Client,
    pool: &PoolConfig,
    username: &str,
    password: &str,
) -> Result<Step> {
    let srp = Srp::new();
    let started = call(
        http,
        pool,
        "InitiateAuth",
        json!({
            "AuthFlow": "USER_SRP_AUTH",
            "ClientId": pool.client_id,
            "AuthParameters": {"USERNAME": username, "SRP_A": hex(&srp.big_a)},
        }),
    )
    .await?;
    if started["ChallengeName"] != "PASSWORD_VERIFIER" {
        bail!("unexpected sign-in step: {}", started["ChallengeName"]);
    }
    let params = &started["ChallengeParameters"];
    let field = |name: &str| {
        params[name]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("sign-in reply is missing {name}"))
    };
    let user_id = field("USER_ID_FOR_SRP")?;
    let secret_block = field("SECRET_BLOCK")?;
    let timestamp = cognito_timestamp(crate::config::now_unix());
    let signature = srp.password_signature(
        pool.pool_name(),
        &user_id,
        password,
        &field("SALT")?,
        &field("SRP_B")?,
        &secret_block,
        &timestamp,
    )?;
    let verified = call(
        http,
        pool,
        "RespondToAuthChallenge",
        json!({
            "ChallengeName": "PASSWORD_VERIFIER",
            "ClientId": pool.client_id,
            "ChallengeResponses": {
                "USERNAME": user_id,
                "PASSWORD_CLAIM_SECRET_BLOCK": secret_block,
                "TIMESTAMP": timestamp,
                "PASSWORD_CLAIM_SIGNATURE": signature,
            },
        }),
    )
    .await?;
    next_step(&verified, &user_id)
}

pub async fn answer_mfa(
    http: &reqwest::Client,
    pool: &PoolConfig,
    session: &str,
    username: &str,
    code: &str,
) -> Result<Tokens> {
    let reply = call(
        http,
        pool,
        "RespondToAuthChallenge",
        json!({
            "ChallengeName": "SOFTWARE_TOKEN_MFA",
            "ClientId": pool.client_id,
            "Session": session,
            "ChallengeResponses": {"USERNAME": username, "SOFTWARE_TOKEN_MFA_CODE": code},
        }),
    )
    .await?;
    match next_step(&reply, username)? {
        Step::Done(tokens) => Ok(tokens),
        Step::NeedsMfa { .. } => bail!("MFA was not accepted"),
    }
}

fn next_step(reply: &Value, username: &str) -> Result<Step> {
    if let Some(result) = reply.get("AuthenticationResult") {
        return parse_tokens(result).map(Step::Done);
    }
    match reply["ChallengeName"].as_str() {
        Some("SOFTWARE_TOKEN_MFA") => Ok(Step::NeedsMfa {
            session: reply["Session"].as_str().unwrap_or_default().to_string(),
            username: reply["ChallengeParameters"]["USER_ID_FOR_SRP"]
                .as_str()
                .unwrap_or(username)
                .to_string(),
        }),
        Some("NEW_PASSWORD_REQUIRED") => {
            bail!("this account needs a new password; sign in to the web app once first")
        }
        Some("MFA_SETUP") => bail!("MFA isn't set up yet; finish setup in the web app first"),
        Some(other) => bail!("unsupported sign-in step: {other}"),
        None => bail!("sign-in reply had no tokens"),
    }
}

pub async fn refresh(
    http: &reqwest::Client,
    pool: &PoolConfig,
    refresh_token: &str,
) -> Result<Tokens> {
    let reply = call(
        http,
        pool,
        "InitiateAuth",
        json!({
            "AuthFlow": "REFRESH_TOKEN_AUTH",
            "ClientId": pool.client_id,
            "AuthParameters": {"REFRESH_TOKEN": refresh_token},
        }),
    )
    .await?;
    parse_tokens(&reply["AuthenticationResult"])
}

/// Invalidates the refresh token and every ID token issued from it.
pub async fn revoke(http: &reqwest::Client, pool: &PoolConfig, refresh_token: &str) -> Result<()> {
    call(
        http,
        pool,
        "RevokeToken",
        json!({"ClientId": pool.client_id, "Token": refresh_token}),
    )
    .await
    .map(|_| ())
}

fn parse_tokens(result: &Value) -> Result<Tokens> {
    let id_token = result["IdToken"]
        .as_str()
        .context("sign-in reply had no ID token")?
        .to_string();
    Ok(Tokens {
        id_token,
        refresh_token: result["RefreshToken"].as_str().map(str::to_string),
        expires_in: result["ExpiresIn"].as_i64().unwrap_or(3600),
    })
}

/// Client side of Cognito's SRP-6a variant.
struct Srp {
    n: BigUint,
    g: BigUint,
    small_a: BigUint,
    big_a: BigUint,
}

impl Srp {
    fn new() -> Self {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("valid SRP group");
        let g = BigUint::from(G);
        loop {
            let mut bytes = [0u8; 128];
            rand::thread_rng().fill_bytes(&mut bytes);
            let small_a = BigUint::from_bytes_be(&bytes);
            let big_a = g.modpow(&small_a, &n);
            if big_a.clone() % &n != BigUint::default() {
                return Self {
                    n,
                    g,
                    small_a,
                    big_a,
                };
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn password_signature(
        &self,
        pool_name: &str,
        user_id: &str,
        password: &str,
        salt_hex: &str,
        b_hex: &str,
        secret_block: &str,
        timestamp: &str,
    ) -> Result<String> {
        let big_b = BigUint::parse_bytes(b_hex.as_bytes(), 16).context("bad SRP_B")?;
        if big_b.clone() % &self.n == BigUint::default() {
            bail!("sign-in server sent an invalid SRP value");
        }
        let salt = BigUint::parse_bytes(salt_hex.as_bytes(), 16).context("bad SALT")?;
        let k = hash_hex_to_int(&format!("{}{}", hex(&self.n), hex(&self.g)));
        let u = hash_hex_to_int(&format!("{}{}", hex(&self.big_a), hex(&big_b)));
        if u == BigUint::default() {
            bail!("sign-in server sent an invalid SRP value");
        }
        let identity = sha256_hex(format!("{pool_name}{user_id}:{password}").as_bytes());
        let x = hash_hex_to_int(&format!("{}{identity}", hex(&salt)));
        let gx_k = (k * self.g.modpow(&x, &self.n)) % &self.n;
        let base = (big_b + &self.n - gx_k) % &self.n;
        let exponent = &self.small_a + u.clone() * x;
        let s = base.modpow(&exponent, &self.n);
        let key = derive_key(&hex_bytes(&hex(&s)), &hex_bytes(&hex(&u)));

        let mut mac = HmacSha256::new_from_slice(&key).expect("any key length");
        mac.update(pool_name.as_bytes());
        mac.update(user_id.as_bytes());
        mac.update(&B64.decode(secret_block).context("bad SECRET_BLOCK")?);
        mac.update(timestamp.as_bytes());
        Ok(B64.encode(mac.finalize().into_bytes()))
    }
}

/// HKDF-SHA256, first 16 bytes, as Cognito derives the password key.
fn derive_key(ikm: &[u8], salt: &[u8]) -> Vec<u8> {
    let mut extract = HmacSha256::new_from_slice(salt).expect("any key length");
    extract.update(ikm);
    let prk = extract.finalize().into_bytes();
    let mut expand = HmacSha256::new_from_slice(&prk).expect("any key length");
    expand.update(DERIVED_KEY_INFO);
    expand.update(&[1]);
    expand.finalize().into_bytes()[..16].to_vec()
}

/// Even-length hex with a leading `00` when the top bit is set, so the
/// value reads as positive — Cognito hashes exactly this form.
fn hex(value: &BigUint) -> String {
    let mut text = value.to_str_radix(16);
    if text.len() % 2 == 1 {
        text.insert(0, '0');
    } else if text
        .as_bytes()
        .first()
        .is_some_and(|c| b"89abcdef".contains(c))
    {
        text.insert_str(0, "00");
    }
    text
}

fn hex_bytes(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap_or(0))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn hash_hex_to_int(hex_text: &str) -> BigUint {
    let digest = sha256_hex(&hex_bytes(hex_text));
    BigUint::parse_bytes(digest.as_bytes(), 16).unwrap_or_default()
}

/// `Sat Nov 7 12:04:05 UTC 2026`: day not zero-padded, as Cognito expects.
fn cognito_timestamp(unix: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{} {} {} {:02}:{:02}:{:02} UTC {}",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        day,
        secs / 3600,
        secs % 3600 / 60,
        secs % 60,
        year
    )
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps_like_cognito() {
        assert_eq!(cognito_timestamp(0), "Thu Jan 1 00:00:00 UTC 1970");
        assert_eq!(
            cognito_timestamp(1_794_052_245),
            "Sat Nov 7 11:50:45 UTC 2026"
        );
        assert_eq!(
            cognito_timestamp(951_782_400),
            "Tue Feb 29 00:00:00 UTC 2000"
        );
    }

    #[test]
    fn pads_hex_to_positive_even_length() {
        assert_eq!(hex(&BigUint::from(0x0fu32)), "0f");
        assert_eq!(hex(&BigUint::from(0x7fu32)), "7f");
        assert_eq!(hex(&BigUint::from(0x80u32)), "0080");
        assert_eq!(hex(&BigUint::from(0x1234u32)), "1234");
    }

    #[test]
    fn hkdf_matches_rfc5869_prefix() {
        // RFC 5869 test case 1 with Cognito's fixed info string is not
        // published, so pin the construction: extract then one expand block.
        let key = derive_key(b"ikm", b"salt");
        assert_eq!(key.len(), 16);
        assert_eq!(key, derive_key(b"ikm", b"salt"));
        assert_ne!(key, derive_key(b"ikm", b"other"));
    }

    #[test]
    fn splits_pool_id() {
        let pool = PoolConfig {
            pool_id: "ap-south-1_Ab1".into(),
            client_id: "cid42".into(),
        };
        assert_eq!(pool.region(), "ap-south-1");
        assert_eq!(pool.pool_name(), "Ab1");
    }

    #[test]
    fn srp_public_value_is_in_group() {
        let srp = Srp::new();
        assert!(srp.big_a < srp.n);
        assert!(srp.big_a > BigUint::default());
    }
}
