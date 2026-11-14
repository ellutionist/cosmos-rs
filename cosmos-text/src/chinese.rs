use crate::{utf8, work, MAX_OUTPUT};
use cosmos::userdata::LuaBytes;
use ferrous_opencc::{config::BuiltinConfig, OpenCC};
use std::sync::OnceLock;
static T2S: OnceLock<Result<OpenCC, String>> = OnceLock::new();
static S2T: OnceLock<Result<OpenCC, String>> = OnceLock::new();
pub(super) struct Chinese;
#[cosmos::userdata(name = "text.chinese")]
impl Chinese {
    async fn convert(text: LuaBytes, config: String) -> Result<String, String> {
        // OpenCC 没有流式受限输出接口；限制原文并保留完整词组上下文。
        if text.len() > 1024 * 1024 {
            return Err("Chinese conversion input exceeds 1 MiB".into());
        }
        let (cache, config) = match config.as_str() {
            "t2s" => (&T2S, BuiltinConfig::T2s),
            "s2t" => (&S2T, BuiltinConfig::S2t),
            _ => return Err("Chinese conversion config must be t2s or s2t".into()),
        };
        work::run(move || {
            let bytes = text.to_vec();
            move |cancel| {
                let text = utf8(bytes)?;
                let converter = cache
                    .get_or_init(|| {
                        OpenCC::from_config(config).map_err(|_| {
                            "failed to load built-in Chinese conversion dictionaries".to_owned()
                        })
                    })
                    .as_ref()
                    .map_err(Clone::clone)?;
                work::check(cancel)?;
                let out = converter.convert(&text);
                work::check(cancel)?;
                if out.len() > MAX_OUTPUT {
                    return Err("text output exceeds 16 MiB".into());
                }
                Ok(out)
            }
        })
        .await
    }
}
