//! 有界多生产者、多消费者 channel；只在同步区间借用队列。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use tokio::sync::Notify;

use crate::userdata::LuaValue;

use super::{Count, Timeout};

#[derive(Clone)]
pub(super) struct Channel(Rc<Inner>);

struct Inner {
    capacity: usize,
    state: RefCell<State>,
    readable: Notify,
    writable: Notify,
}

struct State {
    queue: VecDeque<LuaValue>,
    closed: bool,
}

#[cosmos::userdata(name = "sync.channel")]
impl Channel {
    fn new(capacity: Count) -> Self {
        Self(Rc::new(Inner {
            capacity: capacity.0,
            state: RefCell::new(State {
                queue: VecDeque::new(),
                closed: false,
            }),
            readable: Notify::new(),
            writable: Notify::new(),
        }))
    }
    async fn send(self, value: LuaValue, timeout: Timeout) -> Result<bool, &'static str> {
        timeout
            .wait(async {
                loop {
                    let ready = self.0.writable.notified();
                    tokio::pin!(ready);
                    // 在检查队列之前登记，避免多个发送者漏掉连续通知。
                    ready.as_mut().enable();
                    {
                        let mut state = self.0.state.borrow_mut();
                        if state.closed {
                            return Err("closed");
                        }
                        if state.queue.len() < self.0.capacity {
                            state.queue.push_back(value);
                            self.0.readable.notify_one();
                            return Ok(true);
                        }
                    }
                    ready.await;
                }
            })
            .await
    }

    /// 关闭后仍可读完缓冲区；成功返回一个值（允许 nil），失败返回 nil, err。
    async fn recv(self, timeout: Timeout) -> Result<LuaValue, &'static str> {
        timeout
            .wait(async {
                loop {
                    let ready = self.0.readable.notified();
                    tokio::pin!(ready);
                    ready.as_mut().enable();
                    {
                        let mut state = self.0.state.borrow_mut();
                        if let Some(value) = state.queue.pop_front() {
                            self.0.writable.notify_one();
                            return Ok(value);
                        }
                        if state.closed {
                            return Err("closed");
                        }
                    }
                    ready.await;
                }
            })
            .await
    }

    fn close(&self) {
        self.0.state.borrow_mut().closed = true;
        self.0.readable.notify_waiters();
        self.0.writable.notify_waiters();
    }

    fn len(&self) -> i64 {
        self.0.state.borrow().queue.len() as i64
    }
}
