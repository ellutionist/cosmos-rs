//! 只管理监听与连接/请求的完成通知，不决定业务任务停止顺序。
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};
use tokio::{
    sync::{watch, Notify},
    task::{AbortHandle, JoinHandle},
};
pub(super) struct Life {
    accept: RefCell<Option<JoinHandle<()>>>,
    running: Cell<bool>,
    next: Cell<u64>,
    connections: RefCell<HashMap<u64, Option<AbortHandle>>>,
    requests: Cell<usize>,
    drain: watch::Sender<bool>,
    force: watch::Sender<bool>,
    changed: Notify,
}
enum Kind {
    Accept,
    Connection(u64),
    Request,
}
pub(super) struct Guard {
    life: Rc<Life>,
    kind: Kind,
}
impl Drop for Guard {
    fn drop(&mut self) {
        match self.kind {
            Kind::Accept => self.life.running.set(false),
            Kind::Connection(id) => {
                self.life.connections.borrow_mut().remove(&id);
            }
            Kind::Request => self.life.requests.set(self.life.requests.get() - 1),
        }
        self.life.changed.notify_waiters();
    }
}
impl Life {
    pub(super) fn new() -> Rc<Self> {
        Rc::new(Self {
            accept: RefCell::new(None),
            running: Cell::new(false),
            next: Cell::new(0),
            connections: RefCell::new(HashMap::new()),
            requests: Cell::new(0),
            drain: watch::channel(false).0,
            force: watch::channel(false).0,
            changed: Notify::new(),
        })
    }
    pub(super) fn accept_guard(self: &Rc<Self>) -> Guard {
        self.running.set(true);
        Guard {
            life: self.clone(),
            kind: Kind::Accept,
        }
    }
    pub(super) fn install(&self, task: JoinHandle<()>) {
        *self.accept.borrow_mut() = Some(task);
    }
    pub(super) fn close(&self) {
        if let Some(task) = self.accept.borrow().as_ref() {
            task.abort();
        }
    }
    pub(super) fn connection(self: &Rc<Self>) -> (u64, Guard) {
        let id = self.next.get();
        self.next.set(
            id.checked_add(1)
                .expect("HTTP connection identifier exhausted"),
        );
        self.connections.borrow_mut().insert(id, None);
        (
            id,
            Guard {
                life: self.clone(),
                kind: Kind::Connection(id),
            },
        )
    }
    pub(super) fn track(&self, id: u64, task: AbortHandle) {
        if let Some(slot) = self.connections.borrow_mut().get_mut(&id) {
            *slot = Some(task);
        }
    }
    pub(super) fn request(self: &Rc<Self>) -> Guard {
        self.requests.set(self.requests.get() + 1);
        Guard {
            life: self.clone(),
            kind: Kind::Request,
        }
    }
    pub(super) fn draining(&self) -> watch::Receiver<bool> {
        self.drain.subscribe()
    }
    pub(super) fn forcing(&self) -> watch::Receiver<bool> {
        self.force.subscribe()
    }
    pub(super) fn start_drain(&self) {
        self.close();
        self.drain.send_replace(true);
    }
    pub(super) fn force(&self) {
        self.force.send_replace(true);
        for task in self.connections.borrow().values().flatten() {
            task.abort();
        }
    }
    pub(super) fn connections(&self) -> usize {
        self.connections.borrow().len()
    }
    pub(super) async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.running.get()
                && self.connections.borrow().is_empty()
                && self.requests.get() == 0
            {
                return;
            }
            notified.await;
        }
    }
}
