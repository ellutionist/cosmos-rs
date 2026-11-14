//! 增量摘要和 HMAC，均输出原始字节；认证比较由 RustCrypto / subtle 完成。
use crate::work::{self, State};
use cosmos::userdata::LuaBytes;
use hmac::{Hmac as NativeHmac, Mac};
use sha1::Sha1;
use sha2::{Digest as _, Sha256, Sha512};
use subtle::ConstantTimeEq;

const MAX_INPUT: usize = 64 * 1024 * 1024;
fn input(data: &[u8]) -> Result<(), String> {
    if data.len() > MAX_INPUT {
        Err("digest input exceeds 64 MiB; use incremental updates".into())
    } else {
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}
impl Algorithm {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            "sha512" => Ok(Self::Sha512),
            _ => Err("digest must be sha1, sha256 or sha512".into()),
        }
    }
    fn digest(self) -> Box<dyn sha2::digest::DynDigest + Send> {
        match self {
            Self::Sha1 => Box::new(Sha1::new()),
            Self::Sha256 => Box::new(Sha256::new()),
            Self::Sha512 => Box::new(Sha512::new()),
        }
    }
}
#[derive(Clone)]
pub(super) struct Digest(State<Box<dyn sha2::digest::DynDigest + Send>>);

pub(super) async fn sum(name: &str, data: LuaBytes) -> Result<Vec<u8>, String> {
    let algorithm = Algorithm::parse(name)?;
    input(&data)?;
    work::run(move || {
        let data = data.to_vec();
        move || {
            let mut digest = algorithm.digest();
            digest.update(&data);
            Ok(digest.finalize().to_vec())
        }
    })
    .await
}
#[cosmos::userdata(name = "encryption.digest")]
impl Digest {
    fn new(algorithm: &str) -> Result<Self, String> {
        Ok(Self(State::new(Algorithm::parse(algorithm)?.digest())))
    }
    async fn sum(algorithm: String, data: LuaBytes) -> Result<Vec<u8>, String> {
        sum(&algorithm, data).await
    }
    async fn update(self, data: LuaBytes) -> Result<bool, String> {
        input(&data)?;
        self.0
            .apply(move |mut state| {
                let data = data.to_vec();
                move || {
                    state.update(&data);
                    Ok((Some(state), true))
                }
            })
            .await
    }
    async fn finalize(self) -> Result<Vec<u8>, String> {
        self.0
            .apply(|state| move || Ok((None, state.finalize().to_vec())))
            .await
    }
    async fn verify(self, expected: LuaBytes) -> Result<bool, String> {
        if expected.len() > 64 {
            return self.0.apply(|_state| move || Ok((None, false))).await;
        }
        self.0
            .apply(move |state| {
                let expected = expected.to_vec();
                move || Ok((None, bool::from(state.finalize().as_ref().ct_eq(&expected))))
            })
            .await
    }
    fn close(&self) -> bool {
        self.0.close();
        true
    }
    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

enum MacState {
    Sha1(NativeHmac<Sha1>),
    Sha256(NativeHmac<Sha256>),
    Sha512(Box<NativeHmac<Sha512>>),
}
impl MacState {
    fn new(algorithm: &str, key: &[u8]) -> Result<Self, String> {
        if key.len() > 65536 {
            return Err("HMAC key exceeds 64 KiB".into());
        }
        Ok(match Algorithm::parse(algorithm)? {
            Algorithm::Sha1 => Self::Sha1(
                NativeHmac::<Sha1>::new_from_slice(key).expect("HMAC accepts any key length"),
            ),
            Algorithm::Sha256 => Self::Sha256(
                NativeHmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length"),
            ),
            Algorithm::Sha512 => Self::Sha512(Box::new(
                NativeHmac::<Sha512>::new_from_slice(key).expect("HMAC accepts any key length"),
            )),
        })
    }
    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha1(m) => m.update(data),
            Self::Sha256(m) => m.update(data),
            Self::Sha512(m) => m.update(data),
        }
    }
    fn finalize(self) -> Vec<u8> {
        match self {
            Self::Sha1(m) => m.finalize().into_bytes().to_vec(),
            Self::Sha256(m) => m.finalize().into_bytes().to_vec(),
            Self::Sha512(m) => m.finalize().into_bytes().to_vec(),
        }
    }
    fn verify(self, expected: &[u8]) -> bool {
        match self {
            Self::Sha1(m) => m.verify_slice(expected).is_ok(),
            Self::Sha256(m) => m.verify_slice(expected).is_ok(),
            Self::Sha512(m) => m.verify_slice(expected).is_ok(),
        }
    }
}
#[derive(Clone)]
pub(super) struct Hmac(State<MacState>);
#[cosmos::userdata(name = "encryption.hmac")]
impl Hmac {
    fn new(algorithm: &str, key: &[u8]) -> Result<Self, String> {
        Ok(Self(State::new(MacState::new(algorithm, key)?)))
    }
    async fn sum(algorithm: String, key: LuaBytes, data: LuaBytes) -> Result<Vec<u8>, String> {
        input(&data)?;
        let mut mac = MacState::new(&algorithm, &key)?;
        work::run(move || {
            let data = data.to_vec();
            move || {
                mac.update(&data);
                Ok(mac.finalize())
            }
        })
        .await
    }
    async fn update(self, data: LuaBytes) -> Result<bool, String> {
        input(&data)?;
        self.0
            .apply(move |mut state| {
                let data = data.to_vec();
                move || {
                    state.update(&data);
                    Ok((Some(state), true))
                }
            })
            .await
    }
    async fn finalize(self) -> Result<Vec<u8>, String> {
        self.0
            .apply(|state| move || Ok((None, state.finalize())))
            .await
    }
    async fn verify(self, expected: LuaBytes) -> Result<bool, String> {
        if expected.len() > 64 {
            return self.0.apply(|_state| move || Ok((None, false))).await;
        }
        self.0
            .apply(move |state| {
                let expected = expected.to_vec();
                move || Ok((None, state.verify(&expected)))
            })
            .await
    }
    fn close(&self) -> bool {
        self.0.close();
        true
    }
    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}
