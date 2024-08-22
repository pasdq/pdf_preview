use std::sync::Arc;
use tokio::fs;
use tokio::sync::{broadcast, RwLock, mpsc};
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

// 定义一个仅显示时间的结构体，用于日志记录
struct TimeOnly;

impl FormatTime for TimeOnly {
    // 实现格式化时间的方法
    fn format_time(&self, w: &mut dyn std::fmt::Write) -> fmt::Result {
        let now = Local::now();
        write!(w, "{}", now.format("%H:%M:%S"))
    }
}

// 共享状态结构体，包含文件路径和更新状态等信息
struct SharedState {
    file_path: Option<String>,
    no_pdf_logged: bool,
    updating_file: bool,
}

#[tokio::main]
async fn main() {
    // 初始化日志记录器
    tracing_subscriber::fmt()
        .with_timer(TimeOnly)
        .init();

    // 获取本地IP地址并构建服务器地址
    let local_ip = local_ip().expect("Unable to get local IP address");
    let server_address = format!("http://{}:8848", local_ip);

    info!("Starting server on {}", server_address);

    // 创建广播通道，用于在多个任务之间传递消息
    let (tx, _rx) = broadcast::channel::<String>(10);
    let tx = Arc::new(tx);

    // 初始化共享状态
    let shared_state = Arc::new(RwLock::new(SharedState {
        file_path: None,
        no_pdf_logged: false,
        updating_file: false,
    }));

    // 设置HTML路由，返回静态HTML内容
    let html_route = warp::path::end().map(|| {
        Response::builder()
            .header("content-type", "text/html")
            .body(INDEX_HTML)
    });

    // 设置PDF路由，返回当前目录下的PDF文件
    let pdf_route = warp::path("pdf").and(warp::fs::dir("."));

    // 设置内容路由，返回当前加载的PDF文件名
    let content_route = {
        let shared_state = Arc::clone(&shared_state);
        warp::path("content").and_then(move || {
            let shared_state = Arc::clone(&shared_state);
            async move {
                let path = shared_state.read().await.file_path.clone();
                let response = match path {
                    Some(file_name) => {
                        let file_stem = Path::new(&file_name).file_stem().unwrap().to_string_lossy().to_string();
                        warp::reply::json(&file_stem)
                    }
                    None => warp::reply::json(&"No PDF file found".to_string()),
                };
                Ok::<_, warp::Rejection>(response)
            }
        })
    };

    // 设置WebSocket路由，用于实时推送PDF文件更新
    let ws_route = {
        let tx = Arc::clone(&tx);
        warp::path("ws")
            .and(warp::ws())
            .map(move |ws: warp::ws::Ws| {
                let tx = Arc::clone(&tx);
                ws.on_upgrade(move |websocket| client_connection(websocket, tx.subscribe()))
            })
    };

    // 查找目录中的第一个PDF文件，并更新共享状态和广播通道
    {
        let shared_state = Arc::clone(&shared_state);
        let tx = Arc::clone(&tx);

        if let Ok(Some(pdf_path)) = find_first_pdf_in_dir(".").await {
            let pdf_file_name = pdf_path.file_name().unwrap().to_string_lossy().to_string();
            {
                let mut state = shared_state.write().await;
                state.file_path = Some(pdf_file_name.clone());
                state.no_pdf_logged = false;
            }
            let pdf_file_stem = Path::new(&pdf_file_name).file_stem().unwrap().to_string_lossy().to_string();
            let _ = tx.send(pdf_file_stem);
        } else {
            let mut state = shared_state.write().await;
            state.no_pdf_logged = true;
            error!("No PDF file found in directory!");
            let _ = tx.send("No PDF file found".to_string());
        }
    }

    // 设置文件监视器，用于监听目录中的PDF文件变化
    let (watcher_tx, mut watcher_rx) = mpsc::channel(1);
    let shared_state = Arc::clone(&shared_state);
    let tx = Arc::clone(&tx);
    tokio::spawn(async move {
        let mut watcher = RecommendedWatcher::new(
            move |res: NotifyResult<Event>| {
                if let Ok(event) = res {
                    if event.paths.iter().any(|path| path.extension() == Some(std::ffi::OsStr::new("pdf"))) {
                        let _ = watcher_tx.try_send(event);
                    }
                }
            },
            Config::default(),
        )
        .unwrap();

        watcher.watch(Path::new("."), RecursiveMode::NonRecursive).unwrap();

        let mut last_event_time = tokio::time::Instant::now();
        let debounce_duration = tokio::time::Duration::from_millis(100);

        while let Some(event) = watcher_rx.recv().await {
            let now = tokio::time::Instant::now();
            if now - last_event_time < debounce_duration {
                continue;
            }

            last_event_time = now;

            if event.kind.is_remove() {
                let mut state = shared_state.write().await;
                state.file_path = None;
            }

            match find_first_pdf_in_dir(".").await {
                Ok(Some(pdf_path)) => {
                    let pdf_file_name = pdf_path.file_name().unwrap().to_string_lossy().to_string();
                    let mut should_send = false;

                    {
                        let mut state = shared_state.write().await;
                        if state.file_path != Some(pdf_file_name.clone()) {
                            state.file_path = Some(pdf_file_name.clone());
                            should_send = true;
                        }
                        state.no_pdf_logged = false;
                    }

                    if should_send {
                        let pdf_file_stem = Path::new(&pdf_file_name).file_stem().unwrap().to_string_lossy().to_string();
                        let _ = tx.send(pdf_file_stem);
                    }
                }
                Ok(None) => {
                    let mut state = shared_state.write().await;
                    if !state.no_pdf_logged {
                        error!("No PDF file found in directory!");
                        state.no_pdf_logged = true;
                        let _ = tx.send("No PDF file found".to_string());
                    }
                    state.file_path = None;
                }
                Err(err) => {
                    error!("Error reading directory: {:?}", err);
                }
            }

            shared_state.write().await.updating_file = false;
        }
    });

    // 合并所有路由
    let routes = html_route.or(pdf_route).or(content_route).or(ws_route);

    // 启动Warp服务器并绑定到指定地址和端口
    let server = warp::serve(routes).bind(([0, 0, 0, 0], 8848));

    // 设置优雅关机
    let graceful = async {
        signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
        info!("Shutting down server gracefully...");
    };

    tokio::select! {
        _ = server => {},
        _ = graceful => {},
    }

    info!("Server stopped.");
}

// 查找指定目录中的第一个PDF文件
async fn find_first_pdf_in_dir(dir: &str) -> Result<Option<PathBuf>, std::io::Error> {
    for i in 1..=9 {
        info!("This is the {} attempt to locate the PDF files!", i);
        let mut entries = fs::read_dir(dir).await?;
        let mut latest_pdf: Option<(PathBuf, std::time::SystemTime)> = None;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension() == Some(std::ffi::OsStr::new("pdf")) {
                let metadata = metadata(&path).await?;
                let modified_time = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

                if latest_pdf.is_none() || modified_time > latest_pdf.as_ref().unwrap().1 {
                    latest_pdf = Some((path.clone(), modified_time));
                }
            }
        }

        if let Some((pdf_path, _)) = latest_pdf {
            return Ok(Some(pdf_path));
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Ok(None)
}

// 处理客户端WebSocket连接
async fn client_connection(
    ws: warp::filters::ws::WebSocket,
    mut rx: broadcast::Receiver<String>,
) {
    let (mut tx, _) = ws.split();
    while let Ok(new_content) = rx.recv().await {
        if tx.send(Message::text(new_content)).await.is_err() {
            break;
        }
    }
}

// 静态 HTML 页面
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

