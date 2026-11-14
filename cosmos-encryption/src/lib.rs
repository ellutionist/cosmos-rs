//! Rust 原生加密插件，提供认证需要的摘要与非对称算法。
#![warn(unreachable_pub)]

use cosmos::{Machine, Module};
use ed25519_dalek::hazmat::{raw_sign, ExpandedSecretKey};
use ed25519_dalek::{Signature, VerifyingKey};
use rand::rngs::OsRng;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

/// 注册 cosmos.encryption 的摘要、RSA 与 Ed25519 接口。
pub struct Encryption;

impl Module for Encryption {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<Digests>();
        machine.register::<Rsa>();
        machine.register::<Ed25519>();
        machine.register::<SigningKey>();
        Ok(())
    }
}

struct Digests;

#[cosmos::userdata(name = "encryption")]
impl Digests {
    fn sha1(data: &[u8]) -> Vec<u8> {
        Sha1::digest(data).to_vec()
    }
    fn sha256(data: &[u8]) -> Vec<u8> {
        Sha256::digest(data).to_vec()
    }
    fn sha512(data: &[u8]) -> Vec<u8> {
        Sha512::digest(data).to_vec()
    }
}

struct Rsa {
    key: RsaPublicKey,
}

#[cosmos::userdata(name = "encryption.rsa")]
impl Rsa {
    fn new(pem: &str) -> Result<Self, String> {
        RsaPublicKey::from_public_key_pem(pem)
            .or_else(|_| RsaPublicKey::from_pkcs1_pem(pem))
            .map(|key| Self { key })
            .map_err(|error| format!("invalid RSA public key: {error}"))
    }
    fn size(&self) -> i64 {
        self.key.size() as i64
    }

    /// OAEP 与 MGF1 使用相同摘要，默认 SHA-1 对齐 MySQL 完整认证。
    fn encrypt(&self, data: &[u8], algorithm: Option<String>) -> Result<Vec<u8>, String> {
        let padding = match algorithm.as_deref().unwrap_or("sha1") {
            "sha1" => Oaep::new::<Sha1>(),
            "sha256" => Oaep::new::<Sha256>(),
            _ => return Err("OAEP digest must be sha1 or sha256".into()),
        };
        self.key
            .encrypt(&mut OsRng, padding, data)
            .map_err(|error| error.to_string())
    }
}

struct Ed25519;

struct SigningKey {
    expanded: ExpandedSecretKey,
    verifying: VerifyingKey,
}

fn signing_key(material: &[u8]) -> SigningKey {
    let hash = Sha512::digest(material);
    let expanded = ExpandedSecretKey::from_bytes(&hash.into());
    let verifying = VerifyingKey::from(&expanded);
    SigningKey {
        expanded,
        verifying,
    }
}

#[cosmos::userdata(name = "encryption.ed25519")]
impl Ed25519 {
    fn from_seed(seed: &[u8]) -> Result<SigningKey, String> {
        if seed.len() != 32 {
            return Err("Ed25519 seed must contain 32 bytes".into());
        }
        Ok(signing_key(seed))
    }

    /// 直接 SHA-512 扩展私密材料，支持 MariaDB client_ed25519 的密码派生规则。
    /// 公钥始终在内部从该密钥导出，不接受调用方混入不匹配的公钥或随机数前缀。
    fn from_key_material(material: &[u8]) -> SigningKey {
        signing_key(material)
    }

    fn verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<bool, String> {
        let bytes = public_key
            .try_into()
            .map_err(|_| "Ed25519 public key must contain 32 bytes")?;
        let key = VerifyingKey::from_bytes(bytes).map_err(|error| error.to_string())?;
        let signature = Signature::from_slice(signature).map_err(|error| error.to_string())?;
        Ok(key.verify_strict(message, &signature).is_ok())
    }
}

#[cosmos::userdata(name = "encryption.ed25519.key")]
impl SigningKey {
    fn public_key(&self) -> Vec<u8> {
        self.verifying.to_bytes().to_vec()
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        raw_sign::<Sha512>(&self.expanded, message, &self.verifying)
            .to_bytes()
            .to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::pkcs8::{EncodePublicKey, LineEnding};
    use rsa::RsaPrivateKey;

    #[test]
    fn rsa_parses_both_public_key_formats_and_encrypts_oaep() {
        let mut random = rand::rngs::StdRng::seed_from_u64(42);
        let private = RsaPrivateKey::new(&mut random, 1024).unwrap();
        let public = private.to_public_key();
        for pem in [
            public.to_public_key_pem(LineEnding::LF).unwrap(),
            public.to_pkcs1_pem(LineEnding::LF).unwrap(),
        ] {
            let key = Rsa::new(&pem).unwrap();
            for algorithm in ["sha1", "sha256"] {
                let encrypted = key.encrypt(b"a\0b", Some(algorithm.into())).unwrap();
                let padding = if algorithm == "sha1" {
                    Oaep::new::<Sha1>()
                } else {
                    Oaep::new::<Sha256>()
                };
                assert_eq!(private.decrypt(padding, &encrypted).unwrap(), b"a\0b");
            }
        }
    }
}
