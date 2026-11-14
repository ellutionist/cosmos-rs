use std::{rc::Rc, time::Duration};
use tokio::sync::{watch, Mutex};
enum Listener {
    #[cfg(unix)]
    Unix(tokio::signal::unix::Signal),
    #[cfg(windows)]
    CtrlC(tokio::signal::windows::CtrlC),
    #[cfg(windows)]
    CtrlBreak(tokio::signal::windows::CtrlBreak),
}
impl Listener {
    async fn recv(&mut self) -> Option<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(signal) => signal.recv().await,
            #[cfg(windows)]
            Self::CtrlC(signal) => signal.recv().await,
            #[cfg(windows)]
            Self::CtrlBreak(signal) => signal.recv().await,
        }
    }
}
fn listener(name: &str) -> Result<Listener, String> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let kind = match name {
            "interrupt" => SignalKind::interrupt(),
            "terminate" => SignalKind::terminate(),
            "hangup" => SignalKind::hangup(),
            "quit" => SignalKind::quit(),
            "user1" => SignalKind::user_defined1(),
            "user2" => SignalKind::user_defined2(),
            _ => return Err("unsupported signal name".into()),
        };
        signal(kind)
            .map(Listener::Unix)
            .map_err(|err| format!("signal subscription failed: {err}"))
    }
    #[cfg(windows)]
    {
        match name {
            "interrupt" => tokio::signal::windows::ctrl_c().map(Listener::CtrlC),
            "break" => tokio::signal::windows::ctrl_break().map(Listener::CtrlBreak),
            _ => return Err("unsupported signal name".into()),
        }
        .map_err(|err| format!("signal subscription failed: {err}"))
    }
}
struct Inner {
    listener: Mutex<Option<Listener>>,
    closed: watch::Sender<bool>,
    name: String,
}
#[derive(Clone)]
pub(super) struct Subscription(Rc<Inner>);
#[cosmos::userdata(name = "system.signal")]
impl Subscription {
    fn subscribe(name: &str) -> Result<Self, String> {
        let listener = listener(name)?;
        let (closed, _) = watch::channel(false);
        Ok(Self(Rc::new(Inner {
            listener: Mutex::new(Some(listener)),
            closed,
            name: name.to_owned(),
        })))
    }
    async fn recv(self, timeout: Option<Duration>) -> Result<String, String> {
        let mut closed = self.0.closed.subscribe();
        let wait = async {
            if *closed.borrow() {
                return Err("closed".into());
            }
            let mut signal = tokio::select! {biased;_=closed.changed()=>return Err("closed".into()),signal=self.0.listener.lock()=>signal};
            let listener = signal.as_mut().ok_or("closed")?;
            tokio::select! {
                biased;
                _=closed.changed()=>{*signal=None;Err("closed".into())},
                event=listener.recv()=>event.map(|_|self.0.name.clone()).ok_or_else(||"closed".into()),
            }
        };
        match timeout {
            Some(duration) => tokio::time::timeout(duration, wait)
                .await
                .map_err(|_| "timeout".to_owned())?,
            None => wait.await,
        }
    }
    fn close(&self) {
        self.0.closed.send_replace(true);
        if let Ok(mut signal) = self.0.listener.try_lock() {
            *signal = None;
        }
    }
    fn is_closed(&self) -> bool {
        *self.0.closed.borrow()
    }
}
