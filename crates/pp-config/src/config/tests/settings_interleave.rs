//! 跨进程 `settings.yml` 保存的确定性交错回归。
//!
//! `update_settings_fields_returning` 的“先读后写”窗口曾只靠提交前比较防并发：
//! 两个写者可以基于同一份旧文本先后通过比较，后 persist 者把先写者的字段整个覆盖。
//! 修复后所有保存入口在首次读取之前取得同目录 `settings.lock`，持锁覆盖解析、更新、
//! 临时文件写入、比较与 persist；比较降级为防御外部意外修改的第二道防线。
//!
//! 交错编排不依赖 sleep：写线程在 persist 前停到本模块的握手控制点，
//! 主线程以非阻塞 flock 探测 `settings.lock` 是否被持锁来区分两种实现——
//! 新实现下停在控制点的写者必持锁，旧实现无锁文件可停。

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;

use crate::config::{
    EditMode, Settings, read_settings, update_settings, update_settings_fields_returning,
};

/// 握手等待上限：远大于锁获取的 30 秒等待，只在实现倒退时触发。
const HANDSHAKE: Duration = Duration::from_secs(60);

/// 注册在写线程 thread-local 中的握手：persist 前通知主线程并等待放行。
struct Handshake {
    ready: Sender<()>,
    go: Receiver<()>,
}

thread_local! {
    static HANDSHAKE_SLOT: RefCell<Option<Handshake>> = const { RefCell::new(None) };
}

/// `update_settings_fields_returning` 的测试控制点：注册过握手的写线程在
/// persist 前停下等主线程放行；未注册线程（生产路径与主线程）直通。
pub(crate) fn pre_commit() {
    HANDSHAKE_SLOT.with(|slot| {
        if let Some(handshake) = slot.borrow_mut().as_mut() {
            handshake.ready.send(()).ok();
            // 主线程放行前停在临界区内；通道关闭时直接继续，避免收尾悬挂。
            handshake.go.recv().ok();
        }
    });
}

/// 在独立线程内注册握手并执行一次设置保存。
struct Staged {
    handle: std::thread::JoinHandle<Result<()>>,
    ready: Receiver<()>,
    go: Sender<()>,
}

fn staged_update(
    path: &Path,
    update: impl FnOnce(Settings) -> Result<Vec<(&'static str, serde_yaml::Value)>> + Send + 'static,
) -> Staged {
    let (ready_tx, ready_rx) = channel();
    let (go_tx, go_rx) = channel();
    let path = path.to_path_buf();
    let handle = std::thread::spawn(move || {
        HANDSHAKE_SLOT.with(|slot| {
            *slot.borrow_mut() = Some(Handshake {
                ready: ready_tx,
                go: go_rx,
            })
        });
        let result = update_settings_fields_returning(&path, update).map(|_| ());
        HANDSHAKE_SLOT.with(|slot| *slot.borrow_mut() = None);
        result
    });
    Staged {
        handle,
        ready: ready_rx,
        go: go_tx,
    }
}

/// 以非阻塞 flock 探测同目录 `settings.lock` 是否被某个写者持有。
/// 锁文件不存在（旧实现或尚未保存过）视为无人持锁。
fn lock_is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join("settings.lock"),
    ) else {
        return false;
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            while let Err(rustix::io::Errno::INTR) =
                rustix::fs::flock(&file, rustix::fs::FlockOperation::Unlock)
            {}
            false
        }
        Err(rustix::io::Errno::WOULDBLOCK) => true,
        Err(_) => false,
    }
}

#[test]
fn settings_lock_serializes_concurrent_field_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    fs::write(&path, "# 保留注释\nedit: hashline\n").unwrap();

    // 写者 A 停在 persist 前：旧实现无锁照样可停；新实现此时持有 settings.lock。
    let first = staged_update(&path, |_| {
        Ok(vec![("debug", serde_yaml::Value::Bool(true))])
    });
    first
        .ready
        .recv_timeout(HANDSHAKE)
        .expect("写者 A 到达提交前控制点");

    // 写者 B 与 A 并发启动：旧实现越过读取与比较直接停到控制点；
    // 新实现先在 settings.lock 上等待，A 释放前不会读取文件。
    let second = staged_update(&path, |_| {
        Ok(vec![(
            "auto_compact_threshold_percent",
            serde_yaml::Value::Number(42.into()),
        )])
    });

    if lock_is_held(&path) {
        // 新实现：A 持锁提交并释放后，B 才读取到含 debug 的最新文本。
        first.go.send(()).expect("写者 A 仍在等待放行");
        second
            .ready
            .recv_timeout(HANDSHAKE)
            .expect("写者 B 在锁释放后到达控制点");
        second.go.send(()).expect("写者 B 仍在等待放行");
    } else {
        // 旧实现：B 已基于同一份旧文本通过比较；两个 persist 无论先后，
        // 后提交者都覆盖先写字段。
        second
            .ready
            .recv_timeout(HANDSHAKE)
            .expect("写者 B 到达提交前控制点");
        first.go.send(()).expect("写者 A 仍在等待放行");
        second.go.send(()).expect("写者 B 仍在等待放行");
    }
    first.handle.join().unwrap().unwrap();
    second.handle.join().unwrap().unwrap();

    // 无论哪个写者后提交，两个顶层字段都必须保留，注释与既有字段原样。
    let settings = read_settings(&path).unwrap();
    assert!(settings.debug);
    assert_eq!(settings.auto_compact_threshold_percent, 42);
    assert_eq!(settings.edit, EditMode::Hashline);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("# 保留注释"), "{text}");
}

#[test]
fn settings_lock_releases_after_write_and_failure_keeps_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    fs::write(&path, "# 保留注释\ndebug: false\n").unwrap();

    // 写者走完整个临界区：锁随作用域释放，下一写入不必等待。
    let writer = staged_update(&path, |_| {
        Ok(vec![("debug", serde_yaml::Value::Bool(true))])
    });
    writer
        .ready
        .recv_timeout(HANDSHAKE)
        .expect("写者到达提交前控制点");
    writer.go.send(()).expect("写者仍在等待放行");
    writer.handle.join().unwrap().unwrap();

    // 锁已释放：同线程的下一保存立即成功，并保留先写字段与注释。
    update_settings(&path, |_| {
        Ok(("read_max_lines", serde_yaml::Value::Number(33.into())))
    })
    .unwrap();
    let settings = read_settings(&path).unwrap();
    assert!(settings.debug);
    assert_eq!(settings.read_max_lines, 33);

    // 严格校验失败不提交：文件保持原样，且失败路径同样释放锁。
    let before = fs::read_to_string(&path).unwrap();
    let error = update_settings(&path, |_| {
        Ok((
            "auto_compact_threshold_percent",
            serde_yaml::Value::Number(0.into()),
        ))
    })
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("auto_compact_threshold_percent must be between 1 and 99"),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    assert_eq!(
        read_settings(&path).unwrap().auto_compact_threshold_percent,
        80
    );

    // 失败后的下一保存可继续。
    update_settings(&path, |_| {
        Ok(("read_max_bytes", serde_yaml::Value::Number(17.into())))
    })
    .unwrap();
    let settings = read_settings(&path).unwrap();
    assert_eq!(settings.read_max_bytes, 17);
    assert!(fs::read_to_string(&path).unwrap().contains("# 保留注释"));
}
