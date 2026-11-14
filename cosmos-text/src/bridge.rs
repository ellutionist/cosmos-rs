//! 叶子类型组合；顺序表只通过受控 Output 构造。
use cosmos::userdata::{Output, PushLua};
pub(super) struct List<T>(pub(super) Vec<T>);
impl<T: PushLua> PushLua for List<T> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let mut table = output.array(self.0.len());
        for (index, value) in self.0.into_iter().enumerate() {
            table.index(index + 1, value);
        }
        table.finish()
    }
}
