//! Issues a Tokenrein license key (vendor only; not part of the server image).
//!
//!   cargo run --example issue_license -- --key ~/.tokenrein/license-signing-key.pem \
//!       --customer "Acme Corp" --plan team --developers 50 --days 365
//!
//! `--key` is the Ed25519 private key whose public half is `proxy/src/license_pub.pem`. It is
//! never kept in the repository. Leave out `--developers` for no developer limit.

use anyhow::{Context, Result, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use tokenrein::license::{Claims, Plan, verify};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut key, mut customer, mut plan, mut developers, mut days) =
        (None, None, None, None, 365_i64);
    while let Some(a) = args.next() {
        let mut value = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--key" => key = Some(value()?),
            "--customer" => customer = Some(value()?),
            "--plan" => {
                plan = Some(match value()?.as_str() {
                    "team" => Plan::Team,
                    "enterprise" => Plan::Enterprise,
                    other => bail!("--plan is team or enterprise, not {other:?}"),
                })
            }
            "--developers" => developers = Some(value()?.parse().context("--developers")?),
            "--days" => days = value()?.parse().context("--days")?,
            other => bail!("unknown argument {other:?}"),
        }
    }
    let key = key.context("--key <private key PEM file> is required")?;
    let pem = std::fs::read_to_string(&key).with_context(|| format!("reading {key}"))?;
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        iss: "tokenrein".into(),
        sub: customer.context("--customer is required")?,
        plan: plan.context("--plan team|enterprise is required")?,
        developers,
        iat: now,
        exp: now + days * 86_400,
        jti: format!("lic_{now:x}"),
    };
    let signer = EncodingKey::from_ed_pem(pem.as_bytes()).context("--key is not an Ed25519 PEM")?;
    let token = jsonwebtoken::encode(&Header::new(Algorithm::EdDSA), &claims, &signer)?;
    // Check it against the public key built into the server before handing it out.
    verify(&token, tokenrein::license::PUBLIC_KEY)
        .context("the key does not match proxy/src/license_pub.pem")?;
    println!("{token}");
    Ok(())
}
