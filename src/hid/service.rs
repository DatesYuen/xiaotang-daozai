//! 【新增】独立 HID 工作线程；开关、限频、取消与界面通信不依赖蓝牙监听。
use super::{Failure, Snapshot};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

pub struct Service {
    pub rx: mpsc::Receiver<Arc<Snapshot>>,
    refresh: mpsc::SyncSender<()>,
    enabled: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}
impl Service {
    pub fn spawn(enabled: bool) -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        let (refresh, requests) = mpsc::sync_channel(1);
        let enabled = Arc::new(AtomicBool::new(enabled));
        let stopped = Arc::new(AtomicBool::new(false));
        let (active, stop) = (enabled.clone(), stopped.clone());
        let startup_error = tx.clone();
        let worker = std::thread::Builder::new()
            .name("daozai-hid-battery".into())
            .spawn(move || {
                let mut last = None;
                let mut generation = 0u64;
                let mut previous_enabled = active.load(Ordering::Relaxed);
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let requested =
                        matches!(requests.recv_timeout(Duration::from_millis(250)), Ok(()));
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let now = Instant::now();
                    let enabled = active.load(Ordering::Relaxed);
                    let changed = enabled != previous_enabled;
                    previous_enabled = enabled;
                    if !should_scan(last, now, enabled, requested, changed) {
                        continue;
                    }
                    generation += 1;
                    #[cfg(windows)]
                    let mut snapshot = super::windows::collect(enabled, &active, &stop, generation);
                    #[cfg(not(windows))]
                    let mut snapshot = Snapshot {
                        error: Some(Failure::Unsupported),
                        ..Default::default()
                    };
                    snapshot.generation = generation;
                    snapshot.collected_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    last = Some(Instant::now());
                    if matches!(
                        tx.try_send(Arc::new(snapshot)),
                        Err(mpsc::TrySendError::Disconnected(_))
                    ) {
                        return;
                    }
                }
            });
        // 【新增】线程创建失败也返回可反馈状态，避免设备管理页永久停在“正在获取”。
        if worker.is_err() {
            let _ = startup_error.try_send(Arc::new(Snapshot {
                generation: 1,
                error: Some(Failure::Io),
                ..Default::default()
            }));
        }
        Self {
            rx,
            refresh,
            enabled,
            stopped,
        }
    }
    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::Relaxed) != enabled {
            let _ = self.refresh.try_send(());
        }
    }
    /// 请求可合并，连续点击不创建新线程或堆积无限扫描。
    pub fn refresh(&self) {
        let _ = self.refresh.try_send(());
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = self.refresh.try_send(());
    }
}
fn should_scan(
    last: Option<Instant>,
    now: Instant,
    enabled: bool,
    requested: bool,
    changed: bool,
) -> bool {
    if changed {
        return true;
    }
    let elapsed = last.map(|at| now.saturating_duration_since(at));
    if requested {
        return elapsed.is_none_or(|age| age >= Duration::from_secs(5));
    }
    enabled && elapsed.is_none_or(|age| age >= Duration::from_secs(60))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn polling_is_bounded_and_switches_override_cooldown() {
        let now = Instant::now();
        assert!(should_scan(None, now, true, false, false));
        assert!(!should_scan(Some(now), now, true, true, false));
        assert!(should_scan(
            Some(now),
            now + Duration::from_secs(5),
            true,
            true,
            false
        ));
        assert!(!should_scan(
            Some(now),
            now + Duration::from_secs(59),
            true,
            false,
            false
        ));
        assert!(should_scan(
            Some(now),
            now + Duration::from_secs(60),
            true,
            false,
            false
        ));
        assert!(!should_scan(None, now, false, false, false));
        assert!(should_scan(Some(now), now, false, false, true));
    }
    #[test]
    fn drop_wakes_and_cancels_worker() {
        let (_, rx) = mpsc::sync_channel(1);
        let (refresh, _) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        drop(Service {
            rx,
            refresh,
            enabled: Arc::new(AtomicBool::new(true)),
            stopped: stopped.clone(),
        });
        assert!(stopped.load(Ordering::Relaxed));
    }
}
