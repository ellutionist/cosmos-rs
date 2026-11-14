//! Argon2 PHC 密码存储；先校验不可信 PHC 的成本，再分配受限的原生工作区。
use crate::{random, work};
use argon2::password_hash::{Output, ParamsString, PasswordHash, SaltString};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use cosmos::userdata::LuaBytes;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub(super) struct Password;
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct Options {
    memory_kib: Option<f64>,
    iterations: Option<f64>,
    parallelism: Option<f64>,
    output_length: Option<f64>,
}
fn params(options: Options) -> Result<Params, String> {
    let memory = random::integer(
        options.memory_kib.unwrap_or(19456.0),
        8.0,
        65536.0,
        "Argon2 memory (KiB)",
    )? as u32;
    let iterations = random::integer(
        options.iterations.unwrap_or(2.0),
        1.0,
        10.0,
        "Argon2 iterations",
    )? as u32;
    let parallelism = random::integer(
        options.parallelism.unwrap_or(1.0),
        1.0,
        4.0,
        "Argon2 parallelism",
    )? as u32;
    let length = random::integer(
        options.output_length.unwrap_or(32.0),
        16.0,
        64.0,
        "Argon2 output length",
    )? as usize;
    Params::new(memory, iterations, parallelism, Some(length))
        .map_err(|err| format!("invalid Argon2 parameters: {err}"))
}
fn input(password: &[u8]) -> Result<(), String> {
    if password.len() > 4096 {
        Err("password exceeds 4096 bytes".into())
    } else {
        Ok(())
    }
}
fn parse(encoded: &str) -> Result<(Algorithm, Version, Params), String> {
    if encoded.len() > 512 {
        return Err("password hash exceeds 512 bytes".into());
    }
    let hash = PasswordHash::new(encoded).map_err(|err| format!("invalid password hash: {err}"))?;
    let algorithm = Algorithm::try_from(hash.algorithm)
        .map_err(|err| format!("unsupported password hash: {err}"))?;
    let version = Version::try_from(hash.version.ok_or("password hash is missing its version")?)
        .map_err(|err| format!("unsupported Argon2 version: {err}"))?;
    if hash.params.iter().count() != 3
        || ["m", "t", "p"]
            .iter()
            .any(|name| hash.params.get(*name).is_none())
    {
        return Err("password hash must contain only m, t and p parameters".into());
    }
    let params =
        Params::try_from(&hash).map_err(|err| format!("invalid Argon2 parameters: {err}"))?;
    let length = hash
        .hash
        .ok_or("password hash is missing its output")?
        .len();
    if params.m_cost() > 65536
        || params.t_cost() > 10
        || params.p_cost() > 4
        || !(16..=64).contains(&length)
    {
        return Err("password hash exceeds supported Argon2 cost limits".into());
    }
    let salt = hash.salt.ok_or("password hash is missing its salt")?;
    let mut bytes = [0; 64];
    if salt
        .decode_b64(&mut bytes)
        .map_err(|err| format!("invalid password salt: {err}"))?
        .len()
        < 8
    {
        return Err("password salt must contain at least 8 bytes".into());
    }
    Ok((algorithm, version, params))
}
fn compute(
    password: &[u8],
    algorithm: Algorithm,
    version: Version,
    params: Params,
    salt: &[u8],
) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut output = Zeroizing::new(vec![0; params.output_len().unwrap_or(32)]);
    // 原生库默认的分配 API 不承诺清除全部工作区，这里显式持有并擦除内存块。
    let mut memory = Zeroizing::new(vec![Block::new(); params.block_count()]);
    Argon2::new(algorithm, version, params)
        .hash_password_into_with_memory(password, salt, &mut output, &mut *memory)
        .map_err(|err| format!("password hashing failed: {err}"))?;
    Ok(output)
}
#[cosmos::userdata(name = "encryption.password")]
impl Password {
    async fn hash(password: LuaBytes, options: Option<Options>) -> Result<String, String> {
        input(&password)?;
        let params = params(options.unwrap_or_default())?;
        work::run(move || {
            let password = Zeroizing::new(password.to_vec());
            move || {
                let mut salt = [0; 16];
                random::fill(&mut salt)?;
                let encoded_salt = SaltString::encode_b64(&salt)
                    .map_err(|err| format!("invalid password salt: {err}"))?;
                let output = compute(
                    &password,
                    Algorithm::Argon2id,
                    Version::V0x13,
                    params.clone(),
                    &salt,
                )?;
                Ok(PasswordHash {
                    algorithm: Algorithm::Argon2id.ident(),
                    version: Some(0x13),
                    params: ParamsString::try_from(&params)
                        .map_err(|err| format!("invalid Argon2 parameters: {err}"))?,
                    salt: Some(encoded_salt.as_salt()),
                    hash: Some(
                        Output::new(&output)
                            .map_err(|err| format!("invalid password hash output: {err}"))?,
                    ),
                }
                .to_string())
            }
        })
        .await
    }
    async fn verify(password: LuaBytes, encoded: String) -> Result<bool, String> {
        input(&password)?;
        let (algorithm, version, params) = parse(&encoded)?;
        work::run(move || {
            let password = Zeroizing::new(password.to_vec());
            move || {
                let hash = PasswordHash::new(&encoded)
                    .map_err(|err| format!("invalid password hash: {err}"))?;
                let mut salt = [0; 64];
                let salt = hash
                    .salt
                    .ok_or("password hash is missing its salt")?
                    .decode_b64(&mut salt)
                    .map_err(|err| format!("invalid password salt: {err}"))?;
                let output = compute(&password, algorithm, version, params, salt)?;
                Ok(bool::from(
                    output.ct_eq(
                        hash.hash
                            .ok_or("password hash is missing its output")?
                            .as_bytes(),
                    ),
                ))
            }
        })
        .await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use argon2::{PasswordHasher, PasswordVerifier};
    #[test]
    fn phc_compatibility_and_resource_limits() {
        let params = Params::new(32, 2, 1, Some(32)).unwrap();
        let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
        for algorithm in [Algorithm::Argon2id, Algorithm::Argon2i, Algorithm::Argon2d] {
            for version in [Version::V0x10, Version::V0x13] {
                let native = Argon2::new(algorithm, version, params.clone());
                let encoded = native
                    .hash_password(b"pass\0word", &salt)
                    .unwrap()
                    .to_string();
                let (a, v, p) = parse(&encoded).unwrap();
                let output = compute(b"pass\0word", a, v, p, b"0123456789abcdef").unwrap();
                let hash = PasswordHash::new(&encoded).unwrap();
                assert_eq!(&*output, hash.hash.unwrap().as_bytes());
                assert!(native.verify_password(b"pass\0word", &hash).is_ok());
            }
        }
        for encoded in [
            "$argon2id$v=19$m=65537,t=2,p=1$c29tZXNhbHQ$YWJjZGVmZ2hpamtsbW5vcA",
            "$argon2id$v=19$m=19456,t=11,p=1$c29tZXNhbHQ$YWJjZGVmZ2hpamtsbW5vcA",
            "$argon2id$v=19$m=19456,t=2,p=5$c29tZXNhbHQ$YWJjZGVmZ2hpamtsbW5vcA",
            "$argon2id$v=19$m=19456,t=2$c29tZXNhbHQ$YWJjZGVmZ2hpamtsbW5vcA",
        ] {
            assert!(parse(encoded).is_err(), "accepted unsafe PHC: {encoded}");
        }
    }
}
