use std::sync::Arc;
use tokio::fs;
use tokio::sync::{broadcast, RwLock};
use tokio::signal;
use warp::filters::ws::Message;
use warp::{http::Response, Filter};
use futures_util::{StreamExt, SinkExt};
use std::path::{Path, PathBuf};
use tracing::{info, error};
use tracing_subscriber::fmt::time::FormatTime;
use std::fmt;
use chrono::Local;
use notify::{RecommendedWatcher, RecursiveMode, Watcher, Event, Config, Result as NotifyResult};
use tokio::fs::metadata;
use local_ip_address::local_ip;

struct TimeOnly;

impl FormatTime for TimeOnly {
    fn format_time(&self, w: &mut dyn std::fmt::Write) -> fmt::Result {
        let now = Local::now();
        write!(w, "{}", now.format("%H:%M:%S"))
    }
}

#[tokio::main]
async fn main() {
    // 设置日志系统，只显示时间
    tracing_subscriber::fmt()
        .with_timer(TimeOnly)
        .init();

    // 获取本机局域网 IP 地址
    let local_ip = local_ip().expect("Unable to get local IP address");
    let server_address = format!("http://{}:8848", local_ip);

    // 提示服务器启动
    info!("Starting server on {}", server_address);

    // 创建一个广播通道用于文件更新通知
    let (tx, _rx) = broadcast::channel::<String>(10);
    let tx = Arc::new(tx);

    // 共享的文件路径和状态
    let file_path = Arc::new(RwLock::new(None));
    let no_pdf_logged = Arc::new(RwLock::new(false));
    let updating_file = Arc::new(RwLock::new(false));

    // 提供 HTML 页面
    let html_route = warp::path::end().map(|| {
        Response::builder()
            .header("content-type", "text/html")
            .body(INDEX_HTML)
    });

    // 提供 PDF 文件的静态文件服务
    let pdf_route = warp::path("pdf").and(warp::fs::dir("."));

    // 提供当前最新的 PDF 文件名
    let content_route = {
        let file_path = Arc::clone(&file_path);
        warp::path("content").and_then(move || {
            let file_path = Arc::clone(&file_path);
            async move {
                let path = file_path.read().await;
                let response = match &*path {
                    Some(file_name) => {
                        let file_stem = Path::new(file_name).file_stem().unwrap().to_string_lossy().to_string();
                        warp::reply::json(&file_stem)
                    }
                    None => warp::reply::json(&"No PDF file found".to_string()),
                };
                Ok::<_, warp::Rejection>(response)
            }
        })
    };

    // WebSocket 路由用于实时更新
    let ws_route = {
        let tx = Arc::clone(&tx);
        warp::path("ws")
            .and(warp::ws())
            .map(move |ws: warp::ws::Ws| {
                let tx = Arc::clone(&tx);
                ws.on_upgrade(move |websocket| client_connection(websocket, tx.subscribe()))
            })
    };

    // 在启动时检查当前目录中的PDF文件
    {
        let file_path_arc = Arc::clone(&file_path);
        let no_pdf_logged_arc = Arc::clone(&no_pdf_logged);
        let tx = Arc::clone(&tx);

        if let Ok(Some(pdf_path)) = find_first_pdf_in_dir(".").await {
            let pdf_file_name = pdf_path.file_name().unwrap().to_string_lossy().to_string();
            {
                let mut path = file_path_arc.write().await;
                *path = Some(pdf_file_name.clone());
            }

            let pdf_file_stem = Path::new(&pdf_file_name).file_stem().unwrap().to_string_lossy().to_string();
            let _ = tx.send(pdf_file_stem);
        } else {
            let mut no_pdf_logged = no_pdf_logged_arc.write().await;
            *no_pdf_logged = true;
            error!("No PDF file found in directory!");
            let _ = tx.send("No PDF file found".to_string());
        }
    }

    // 创建一个任务监控目录变化
    let file_path_arc = Arc::clone(&file_path);
    let no_pdf_logged_arc = Arc::clone(&no_pdf_logged);
    let updating_file_arc = Arc::clone(&updating_file);
    let tx = Arc::clone(&tx);
    tokio::spawn(async move {
        // 创建一个文件系统监视器
        let (watcher_tx, mut watcher_rx) = tokio::sync::mpsc::channel(1);
        let mut watcher = RecommendedWatcher::new(
            move |res: NotifyResult<Event>| {
                if let Ok(event) = res {
                    // 检查事件是否来自 `temporary` 文件夹或该文件夹内的文件，如果是则忽略
                    if event.paths.iter().any(|path| {
                        path.starts_with("temporary") || path.components().any(|c| c.as_os_str() == "temporary")
                    }) {
                        return; // 忽略来自 `temporary` 文件夹的事件
                    }
                    let _ = watcher_tx.try_send(event);
                }
            },
            Config::default(),
        ).unwrap();

        watcher.watch(Path::new("."), RecursiveMode::NonRecursive).unwrap();

        while let Some(_event) = watcher_rx.recv().await {
            // 标记文件正在更新
            {
                let mut updating = updating_file_arc.write().await;
                *updating = true;
            }

            match find_first_pdf_in_dir(".").await {
                Ok(Some(pdf_path)) => {
                    let pdf_file_name = pdf_path.file_name().unwrap().to_string_lossy().to_string();
                    let mut should_send = false;

                    {
                        let mut path = file_path_arc.write().await;
                        if *path != Some(pdf_file_name.clone()) {
                            *path = Some(pdf_file_name.clone());
                            should_send = true;
                        }
                    }

                    {
                        let mut no_pdf_logged = no_pdf_logged_arc.write().await;
                        *no_pdf_logged = false;
                    }

                    if should_send {
                        let pdf_file_stem = Path::new(&pdf_file_name).file_stem().unwrap().to_string_lossy().to_string();
                        let _ = tx.send(pdf_file_stem);
                    }
                }
                Ok(None) => {
                    {
                        let mut no_pdf_logged = no_pdf_logged_arc.write().await;
                        if !*no_pdf_logged {
                            error!("No PDF file found in directory!");
                            *no_pdf_logged = true;
                            // 不立即发送“没有 PDF 文件”的通知
                        }
                    }
                }
                Err(err) => {
                    error!("Error reading directory: {:?}", err);
                }
            }

            // 取消文件更新标记
            {
                let mut updating = updating_file_arc.write().await;
                *updating = false;
            }
        }
    });

    // 组合路由
    let routes = html_route.or(pdf_route).or(content_route).or(ws_route);

    // 服务器绑定和运行
    let server = warp::serve(routes).bind(([0, 0, 0, 0], 8848));

    // 捕获 Ctrl+C 退出信号
    let graceful = async {
        signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
        info!("Shutting down server gracefully...");
    };

    // 运行服务器和监听退出信号
    tokio::select! {
        _ = server => {},
        _ = graceful => {},
    }

    info!("Server stopped.");
}

async fn find_first_pdf_in_dir(dir: &str) -> Result<Option<PathBuf>, std::io::Error> {
    // 循环十次
    for i in 1..=9 {
        info!("This is the {} attempt to locate the PDF files!", i);  // 输出 "......(i)"，i 表示当前循环次数
        let mut entries = fs::read_dir(dir).await?;
        let mut pdf_files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            // 忽略 `temporary` 文件夹中的文件
            if path.starts_with("temporary") || path.components().any(|c| c.as_os_str() == "temporary") {
                continue;
            }
            if let Some(ext) = path.extension() {
                if ext == "pdf" {
                    let metadata = metadata(&path).await?;
                    pdf_files.push((path, metadata));
                }
            }
        }

        pdf_files.sort_by_key(|&(_, ref metadata)| metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH));
        pdf_files.reverse();

        if !pdf_files.is_empty() {
            return Ok(Some(pdf_files.first().map(|(path, _)| path.clone()).unwrap()));
        }

        // 等待 100 毫秒再进行下一次检查
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Ok(None)
}

async fn client_connection(
    ws: warp::filters::ws::WebSocket,
    mut rx: broadcast::Receiver<String>,
) {
    let (mut tx, _) = ws.split();
    while let Ok(new_content) = rx.recv().await {
        if tx.send(Message::text(new_content)).await.is_err() {
            // WebSocket 连接可能已断开
            break;
        }
    }
}

const INDEX_HTML: &str = r#"
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>PDF Viewer</title>
    <style>
        body, html {
            margin: 0;
            width: 100%;
            height: 100%;
            overflow: hidden;
        }
        iframe {
            width: 100%;
            height: 100%;
            border: none;
        }
    </style>
</head>
<body>
    <iframe id="pdf-frame" src=""></iframe>
    <script>
        const pdfFrame = document.getElementById("pdf-frame");

        async function fetchFileContent() {
            const response = await fetch('/content');
            const fileName = await response.json();
            if (fileName !== 'No PDF file found') {
                const browserType = getBrowserType();
                const zoomParam = browserType === 'Firefox' ? '#zoom=page-width' : '#view=FitH';
                pdfFrame.src = '/pdf/' + fileName + '.pdf' + zoomParam;
            }
        }

        function setupWebSocket() {
            const ws = new WebSocket('ws://' + window.location.host + '/ws');
            ws.onmessage = (event) => {
                const fileName = event.data;
                if (fileName !== 'No PDF file found') {
                    const browserType = getBrowserType();
                    const zoomParam = browserType === 'Firefox' ? '#zoom=page-width' : '#view=FitH';
                    pdfFrame.src = '/pdf/' + fileName + '.pdf' + zoomParam;
                }
            };
        }

        function getBrowserType() {
            const userAgent = navigator.userAgent;
            if (userAgent.indexOf('Firefox') > -1) {
                return 'Firefox';
            } else if (userAgent.indexOf('Chrome') > -1 || userAgent.indexOf('Edg') > -1) {
                return 'Chrome';
            }
            return 'Other';
        }

        fetchFileContent();
        setupWebSocket();
    </script>
</body>
</html>
"#;
