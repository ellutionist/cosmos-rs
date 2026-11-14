use super::handle::Handle;
use super::{blocking, encode_path};
use std::fs::ReadDir;
use std::path::PathBuf;

#[derive(Clone)]
pub(super) struct Directory(Handle<ReadDir>);
#[derive(crate::PushLua)]
struct Entry {
    name: Vec<u8>,
    path: Vec<u8>,
    kind: &'static str,
}
impl Directory {
    pub(super) async fn open(path: PathBuf) -> Result<Self, String> {
        Ok(Self(Handle::new(
            blocking(move || std::fs::read_dir(path)).await?,
        )))
    }
}
#[crate::userdata(name = "fs.directory")]
impl Directory {
    /// 顺序由文件系统决定；遍历结束返回 nil，不一次读入整个目录。
    async fn next(self) -> Result<Option<Entry>, String> {
        self.0
            .run(|entries| {
                entries
                    .next()
                    .transpose()?
                    .map(|entry| {
                        let kind = entry.file_type()?;
                        Ok(Entry {
                            name: encode_path(PathBuf::from(entry.file_name()))?,
                            path: encode_path(entry.path())?,
                            kind: if kind.is_file() {
                                "file"
                            } else if kind.is_dir() {
                                "directory"
                            } else if kind.is_symlink() {
                                "symlink"
                            } else {
                                "other"
                            },
                        })
                    })
                    .transpose()
            })
            .await
    }
    fn settimeout(&self, ms: f64) -> Result<bool, String> {
        self.0.settimeout(ms)
    }
    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
    async fn close(self) -> Result<bool, String> {
        self.0.close().await
    }
}
