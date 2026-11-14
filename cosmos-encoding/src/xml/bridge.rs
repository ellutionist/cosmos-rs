//! 叶子序列转换；节点和属性使用同一组受控表上下文，不接触 Lua 栈。
use super::model::{MAX_BYTES, MAX_NODES};
use cosmos::userdata::{FromLua, Output, PushLua, Value, ValueKind};
use std::collections::BTreeMap;
pub(super) trait InputSize {
    fn input_size(&self) -> usize;
}
pub(super) struct List<T>(pub(super) Vec<T>);
pub(super) struct InputList<T>(pub(super) Result<Vec<T>, String>);
impl<'lua, T: FromLua<'lua> + InputSize> FromLua<'lua> for InputList<T> {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let parsed = (|| {
            let mut items = BTreeMap::new();
            let mut bytes = 0usize;
            value.table()?.entries(|key, value| {
                if items.len() >= MAX_NODES {
                    return Err("XML sequence exceeds node limit".into());
                }
                if key.kind() != ValueKind::Number {
                    return Err("XML sequence requires integer keys".into());
                }
                let index = key.read::<f64>()?;
                if !index.is_finite()
                    || index.fract() != 0.0
                    || !(1.0..=MAX_NODES as f64).contains(&index)
                {
                    return Err("XML sequence requires consecutive integer keys".into());
                }
                let item = value.read::<T>()?;
                bytes = bytes.saturating_add(item.input_size());
                if bytes > MAX_BYTES {
                    return Err("XML sequence input exceeds 16 MiB".into());
                }
                items.insert(index as usize, item);
                Ok(())
            })?;
            if items.keys().copied().ne(1..=items.len()) {
                return Err("XML sequence must not contain holes".into());
            }
            Ok(items.into_values().collect())
        })();
        Ok(Self(parsed))
    }
}
impl<T: PushLua> PushLua for List<T> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let mut table = output.array(self.0.len());
        for (index, value) in self.0.into_iter().enumerate() {
            table.index(index + 1, value);
        }
        table.finish()
    }
}
