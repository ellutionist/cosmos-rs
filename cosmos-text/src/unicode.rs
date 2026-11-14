use crate::{append, input, integer, utf8, work};
use cosmos::userdata::LuaBytes;
use unicode_normalization::UnicodeNormalization;
pub(super) struct Unicode;
fn collect(
    chars: impl Iterator<Item = char>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<String, String> {
    let mut out = String::new();
    for (index, ch) in chars.enumerate() {
        if index % 4096 == 0 {
            work::check(cancel)?;
        }
        append(&mut out, ch.encode_utf8(&mut [0; 4]))?;
    }
    Ok(out)
}
#[cosmos::userdata(name = "text.unicode")]
impl Unicode {
    async fn normalize(text: LuaBytes, form: String) -> Result<String, String> {
        input(&text)?;
        if !matches!(form.as_str(), "nfc" | "nfd" | "nfkc" | "nfkd") {
            return Err("normalization form must be nfc, nfd, nfkc or nfkd".into());
        }
        work::run(move || {
            let bytes = text.to_vec();
            move |cancel| {
                let text = utf8(bytes)?;
                match form.as_str() {
                    "nfc" => collect(text.nfc(), cancel),
                    "nfd" => collect(text.nfd(), cancel),
                    "nfkc" => collect(text.nfkc(), cancel),
                    _ => collect(text.nfkd(), cancel),
                }
            }
        })
        .await
    }
    async fn lower(text: LuaBytes) -> Result<String, String> {
        input(&text)?;
        work::run(move || {
            let bytes = text.to_vec();
            move |cancel| {
                // str 的大小写转换保留上下文相关的希腊语词尾 sigma 语义。
                let out = utf8(bytes)?.to_lowercase();
                work::check(cancel)?;
                if out.len() > crate::MAX_OUTPUT {
                    return Err("text output exceeds 16 MiB".into());
                }
                Ok(out)
            }
        })
        .await
    }
    async fn upper(text: LuaBytes) -> Result<String, String> {
        input(&text)?;
        work::run(move || {
            let bytes = text.to_vec();
            move |cancel| collect(utf8(bytes)?.chars().flat_map(char::to_uppercase), cancel)
        })
        .await
    }
    async fn trim(text: LuaBytes) -> Result<String, String> {
        input(&text)?;
        work::run(move || {
            let bytes = text.to_vec();
            move |_| Ok(utf8(bytes)?.trim().to_owned())
        })
        .await
    }
    async fn len(text: LuaBytes) -> Result<u32, String> {
        input(&text)?;
        work::run(move || {
            let bytes = text.to_vec();
            move |_| Ok(utf8(bytes)?.chars().count() as u32)
        })
        .await
    }
    async fn truncate(text: LuaBytes, max_chars: f64) -> Result<String, String> {
        input(&text)?;
        let max = integer(max_chars, 0, crate::MAX_INPUT, "character limit")?;
        work::run(move || {
            let bytes = text.to_vec();
            move |cancel| collect(utf8(bytes)?.chars().take(max), cancel)
        })
        .await
    }
}
