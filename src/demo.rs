//! #[cosmos::userdata] 使用示例：Counter 类型桥接

pub struct Counter {
    n: i64,
}

#[cosmos::userdata]
impl Counter {
    fn new(start: i64) -> Counter {
        Counter { n: start }
    }

    fn get(&self) -> i64 {
        self.n
    }

    fn inc(&mut self, by: i64) -> i64 {
        self.n += by;
        self.n
    }

    fn set(&mut self, value: i64) {
        self.n = value;
    }

    fn greet(&self, name: &str) -> String {
        format!("hello, {name}! counter = {}", self.n)
    }

    fn try_div(&self, by: i64) -> Result<i64, String> {
        if by == 0 {
            Err("division by zero".to_string())
        } else {
            Ok(self.n / by)
        }
    }

    fn dup(&self) -> Counter {
        Counter { n: self.n }
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        eprintln!("counter dropped (n={})", self.n);
    }
}

/// 启动时注册类型，脚本才能在任何实例之前 require("cosmos.Counter")
pub unsafe fn register(L: *mut cosmos::ffi::lua_State) {
    cosmos::userdata::register::<Counter>(L);
}
