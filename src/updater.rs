//! One update state shared by the main window and tray window. Networking never blocks GPUI.
use std::path::PathBuf;
use std::time::Duration;
use gpui::{Context, Entity, Global};
use crate::{install, release, store};

pub struct Updates(pub Entity<Updater>);
impl Global for Updates {}

const RECHECK: Duration = Duration::from_secs(6 * 60 * 60);

pub enum State {
    /// No newer release known: nothing to show.
    Idle,
    /// A newer release; `error` is why the last attempt to install it failed.
    Available { release: release::Available, error: Option<String> },
    Downloading { release: release::Available },
    Applying { release: release::Available },
}

impl State {
    /// The release the button offers, None when there is none.
    pub fn release(&self) -> Option<&release::Available> {
        match self {
            Self::Idle => None,
            Self::Available { release, .. } | Self::Downloading { release } | Self::Applying { release } => Some(release),
        }
    }
    pub fn busy(&self) -> bool { matches!(self, Self::Downloading { .. } | Self::Applying { .. }) }
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Available { error, .. } => error.as_deref(),
            _ => None,
        }
    }
}

pub struct Updater {
    pub state: State,
    /// The previous run's failed install, shown once the release is offered again.
    failed: Option<String>,
}
fn pending_path() -> PathBuf { store::cache_dir().join("pending-update.json") }

impl Updater {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let failed = store::load::<PathBuf>(&pending_path()).and_then(|stage| {
            let result: Option<std::result::Result<(), String>> = store::load(&stage.join("result.json"));
            let _ = std::fs::remove_file(pending_path());
            match result {
                Some(Ok(())) => None,
                Some(Err(e)) => Some(e),
                None => Some(t!("update.interrupted").into()),
            }
        });
        cx.spawn(async move |this, cx| {
            loop {
                let result = cx.background_executor().spawn(async { release::check() }).await;
                if this.update(cx, |this, cx| this.checked(result, cx)).is_err() { return; }
                cx.background_executor().timer(RECHECK).await;
            }
        }).detach();
        Self { state: State::Idle, failed }
    }

    /// A failed check (offline, development build) keeps what is known.
    fn checked(&mut self, result: Result<Option<release::Available>, String>, cx: &mut Context<Self>) {
        let Ok(found) = result else { return };
        if self.state.busy() || self.state.release().map(|r| &r.version) == found.as_ref().map(|r| &r.version) { return; }
        self.state = match found {
            Some(release) => State::Available { release, error: self.failed.take() },
            None => State::Idle,
        };
        cx.notify();
    }

    /// Download, then install and restart.
    pub fn click(&mut self, cx: &mut Context<Self>) {
        let State::Available { release, .. } = &self.state else { return };
        // Don't exit while an elevated connection start is still in flight.
        if cx.global::<crate::tray::Windows>().vpn.as_ref().is_some_and(|v| v.read(cx).s.conn == crate::app::Conn::Connecting) {
            return;
        }
        let release = release.clone();
        self.state = State::Downloading { release: release.clone() };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let staged = cx.background_executor().spawn({
                let release = release.clone();
                async move { stage(release) }
            }).await;
            let _ = this.update(cx, |this, cx| match staged {
                Ok(plan) => this.apply(release, plan, cx),
                Err(e) => this.failed(release, e, cx),
            });
        }).detach();
    }

    fn failed(&mut self, release: release::Available, error: String, cx: &mut Context<Self>) {
        self.state = State::Available { release, error: Some(error) };
        cx.notify();
    }

    fn apply(&mut self, release: release::Available, plan: PathBuf, cx: &mut Context<Self>) {
        self.state = State::Applying { release: release.clone() };
        let vpn = cx.global::<crate::tray::Windows>().vpn.clone();
        let stopped = vpn.as_ref().map(|v| v.update(cx, |v, cx| v.stop_for_update(cx)));
        cx.notify();
        cx.spawn(async move |this, cx| {
            if let Some(stopped) = stopped { stopped.await; }
            let pending = pending_path();
            let result = cx.background_executor().spawn(async move {
                let stage = plan.parent().ok_or("Invalid update plan.")?.to_path_buf();
                std::fs::create_dir_all(pending.parent().unwrap()).map_err(|e| e.to_string())?;
                std::fs::write(&pending, serde_json::to_vec(&stage).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                let result = install::launch_worker(&plan);
                if result.is_err() { let _ = std::fs::remove_file(pending); }
                result
            }).await;
            match result {
                Ok(()) => { let _ = cx.update(|cx| cx.quit()); }
                Err(e) => {
                    if let Some(vpn) = vpn { let _ = cx.update(|cx| vpn.update(cx, |vpn, _| vpn.cancel_update())); }
                    let _ = this.update(cx, |this, cx| this.failed(release, e, cx));
                }
            }
        }).detach();
    }
}

fn stage(available: release::Available) -> Result<PathBuf, String> {
    let dest = install::detect(&std::env::current_exe().map_err(|e| e.to_string())?)?;
    let stage = release::staging()?;
    let result = (|| {
        let package = stage.join("download");
        release::download(&available.asset, &package)?;
        let unpacked = stage.join("unpacked");
        release::unpack(&package, &unpacked, available.asset.name.ends_with(".zip"))?;
        let version = available.version.to_string();
        let expected = match dest.kind {
            install::Kind::Windows => format!("hitori-{version}-windows-x86_64"),
            install::Kind::Linux => format!("hitori-{version}-linux-x86_64"),
            install::Kind::Macos => "Hitori VPN.app".into(),
        };
        let source = release::package_root(&unpacked, &expected)?;
        let plan = install::prepare_update(source, dest, stage.clone())?;
        let _ = std::fs::remove_file(package);
        Ok(plan)
    })();
    if result.is_err() { let _ = std::fs::remove_dir_all(stage); }
    result
}
