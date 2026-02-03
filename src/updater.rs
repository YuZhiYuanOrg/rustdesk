use crate::{common::do_check_software_update, hbbs_http::create_http_client_with_url};
use hbb_common::{bail, config, log, ResultType};
use std::{
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{channel, Receiver, Sender},
        Mutex,
    },
    time::{Duration, Instant},
};

enum UpdateMsg {
    CheckUpdate,
    Exit,
}

lazy_static::lazy_static! {
    static ref TX_MSG : Mutex<Sender<UpdateMsg>> = Mutex::new(start_auto_update_check());
}

static CONTROLLING_SESSION_COUNT: AtomicUsize = AtomicUsize::new(0);

pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60 * 2);

pub fn update_controlling_session_count(count: usize) {
    CONTROLLING_SESSION_COUNT.store(count, Ordering::SeqCst);
}

#[allow(dead_code)]
pub fn start_auto_update() {
    let _sender = TX_MSG.lock().unwrap();
}

#[allow(dead_code)]
pub fn manually_check_update() -> ResultType<()> {
    let sender = TX_MSG.lock().unwrap();
    sender.send(UpdateMsg::CheckUpdate)?;
    Ok(())
}

#[allow(dead_code)]
pub fn stop_auto_update() {
    let sender = TX_MSG.lock().unwrap();
    sender.send(UpdateMsg::Exit).unwrap_or_default();
}

#[inline]
fn has_no_active_conns() -> bool {
    let conns = crate::Connection::alive_conns();
    conns.is_empty() && has_no_controlling_conns()
}

#[cfg(any(not(target_os = "windows"), feature = "flutter"))]
fn has_no_controlling_conns() -> bool {
    CONTROLLING_SESSION_COUNT.load(Ordering::SeqCst) == 0
}

#[cfg(not(any(not(target_os = "windows"), feature = "flutter")))]
fn has_no_controlling_conns() -> bool {
    let app_exe = format!("{}.exe", crate::get_app_name().to_lowercase());
    for arg in [
        "--connect",
        "--play",
        "--file-transfer",
        "--view-camera",
        "--port-forward",
        "--rdp",
    ] {
        if !crate::platform::get_pids_of_process_with_first_arg(&app_exe, arg).is_empty() {
            return false;
        }
    }
    true
}

fn start_auto_update_check() -> Sender<UpdateMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || start_auto_update_check_(rx));
    return tx;
}

/// 自动更新检查线程的入口函数
///
/// # 参数
/// * `rx_msg` - 用于接收更新控制消息的通道接收端
fn start_auto_update_check_(rx_msg: Receiver<UpdateMsg>) {
    // 初始延迟：启动后先等待5分钟再执行第一次检查
    std::thread::sleep(Duration::from_secs(60 * 5));

    // 执行首次更新检查
    if let Err(e) = check_update() {
        log::error!("Error checking for updates: {}", e);
        updater_log(&format!("Error checking for updates: {}", e));
    }

    // 常量定义
    const MIN_INTERVAL: Duration = Duration::from_secs(60 * 10);  // 最小检查间隔：10分钟
    const RETRY_INTERVAL: Duration = Duration::from_secs(60 * 30);  // 失败重试间隔：30分钟

    // 状态变量初始化
    let mut last_check_time = Instant::now();  // 记录上次成功检查的时间
    let mut check_interval = UPDATE_CHECK_INTERVAL;  // 当前使用的检查间隔

    // 主循环：持续监听更新消息
    loop {
        // 等待消息，超时时间为当前检查间隔
        let recv_res = rx_msg.recv_timeout(check_interval);

        match &recv_res {
            // 两种情况触发更新检查：
            // 1. 收到显式的检查指令 (Ok(UpdateMsg::CheckUpdate))
            // 2. 等待超时，即达到定期检查时间 (Err(_))
            Ok(UpdateMsg::CheckUpdate) | Err(_) => {
                // 检查距离上次成功检查是否已超过最小间隔
                if last_check_time.elapsed() < MIN_INTERVAL {
                    continue;
                }

                // 检查当前是否存在活跃连接
                if !has_no_active_conns() {
                    check_interval = RETRY_INTERVAL;  // 存在连接，延长检查间隔
                    continue;
                }

                // 执行更新检查
                if let Err(e) = check_update() {
                    // 检查失败：记录错误并延长下次检查间隔
                    log::error!("Error checking for updates: {}", e);
                    updater_log(&format!("Error checking for updates: {}", e));
                    check_interval = RETRY_INTERVAL;
                } else {
                    // 检查成功：更新最后检查时间并恢复默认检查间隔
                    last_check_time = Instant::now();
                    check_interval = UPDATE_CHECK_INTERVAL;
                }
            }

            // 收到退出指令，结束循环和线程
            Ok(UpdateMsg::Exit) => break,
        }
    }
}

fn check_update() -> ResultType<()> {
    #[cfg(target_os = "windows")]
    let is_msi = crate::platform::is_msi_installed()?;
    if !do_check_software_update().is_ok() {
        // ignore
        return Ok(());
    }

    let download_url = crate::common::SOFTWARE_UPDATE_URL.lock().unwrap().clone();
    if download_url.is_empty() {
        log::debug!("No update available.");
        updater_log("No update available.");
    } else {
        log::debug!("New version available.");
        updater_log("New version available.");
        let client = create_http_client_with_url(&download_url);
        let Some(file_path) = get_download_file_from_url(&download_url) else {
            updater_log(&format!("Failed to get the file path from the URL: {}", download_url));
            bail!("Failed to get the file path from the URL: {}", download_url);
        };
        let mut is_file_exists = false;
        if file_path.exists() {
            // Check if the file size is the same as the server file size
            // If the file size is the same, we don't need to download it again.
            let file_size = std::fs::metadata(&file_path)?.len();
            let response = client.head(&download_url).send()?;
            if !response.status().is_success() {
                updater_log(&format!("Failed to get the file size: {}", response.status()));
                bail!("Failed to get the file size: {}", response.status());
            }
            let total_size = response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|ct_len| ct_len.to_str().ok())
                .and_then(|ct_len| ct_len.parse::<u64>().ok());
            let Some(total_size) = total_size else {
                updater_log("Failed to get content length");
                bail!("Failed to get content length");
            };
            if file_size == total_size {
                is_file_exists = true;
            } else {
                std::fs::remove_file(&file_path)?;
            }
        }
        if !is_file_exists {
            let response = client.get(&download_url).send()?;
            if !response.status().is_success() {
                updater_log(&format!(
                    "Failed to download the new version file: {}",
                    response.status()
                ));
                bail!(
                    "Failed to download the new version file: {}",
                    response.status()
                );
            }
            let file_data = response.bytes()?;
            let mut file = std::fs::File::create(&file_path)?;
            file.write_all(&file_data)?;
        }
        // We have checked if the `conns`` is empty before, but we need to check again.
        // No need to care about the downloaded file here, because it's rare case that the `conns` are empty
        // before the download, but not empty after the download.
        if has_no_active_conns() {
            #[cfg(target_os = "windows")]
            update_new_version(is_msi, &file_path);
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn update_new_version(is_msi: bool, file_path: &PathBuf) {
    log::debug!(
        "New version is downloaded, update begin, is msi: {is_msi}, file: {:?}",
        file_path.to_str()
    );
    updater_log(&format!(
        "New version is downloaded, update begin, is msi: {is_msi}, file: {:?}",
        file_path.to_str()
    ));
    if let Some(p) = file_path.to_str() {
        if let Some(session_id) = crate::platform::get_current_process_session_id() {
            if is_msi {
                match crate::platform::update_me_msi(p, true) {
                    Ok(_) => {
                        log::debug!("New version updated.");
                        updater_log("New version updated.");
                    }
                    Err(e) => {
                        log::error!(
                            "Failed to install the new msi version: {}",
                            e
                        );
                        updater_log(&format!(
                            "Failed to install the new msi version: {}",
                            e
                        ));
                    }
                }
            } else {
                match crate::platform::launch_privileged_process(
                    session_id,
                    &format!("{} --update", p),
                ) {
                    Ok(h) => {
                        if h.is_null() {
                            log::error!("Failed to update to the new version.");
                            updater_log("Failed to update to the new version.");
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to run the new version: {}", e);
                        updater_log(&format!("Failed to run the new version: {}", e));
                    }
                }
            }
        } else {
            log::error!(
                "Failed to get the current process session id, Error {}",
                std::io::Error::last_os_error()
            );
            updater_log(&format!(
                "Failed to get the current process session id, Error {}",
                std::io::Error::last_os_error()
            ));
        }
    } else {
        // unreachable!()
        log::error!(
            "Failed to convert the file path to string: {}",
            file_path.display()
        );
        updater_log(&format!(
            "Failed to convert the file path to string: {}",
            file_path.display()
        ));
    }
}

pub fn get_download_file_from_url(url: &str) -> Option<PathBuf> {
    let filename = url.split('/').last()?;
    Some(std::env::temp_dir().join(filename))
}

/// 更新服务日志记录函数
///
/// # 参数
/// - `msg`: 要记录的日志消息
fn updater_log(msg: &str) {
    // 获取日志目录路径
    let path = hbb_common::config::Config::log_path();

    // 创建日志目录（如果不存在）
    if let Err(e) = std::fs::create_dir_all(&path) {
        eprintln!("Failed to create log directory {:?}: {}", path, e);
        return;
    }

    // 构建完整的日志文件路径
    let log_path = path.join("updater.log");

    // 以追加模式打开日志文件（如果不存在则创建）
    if let Ok(mut file) =  std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        // 获取当前本地时间
        let now = chrono::Local::now();

        // 将带时间戳的日志信息写入文件
        // 格式：[YYYY-MM-DD HH:MM:SS] 日志消息
        let _ = writeln!(file, "[{}] {}", now.format("%Y-%m-%d %H:%M:%S"), msg);
    }
}
