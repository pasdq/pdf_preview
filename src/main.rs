use std::sync::{Arc, RwLock};
use tokio::fs;
use tokio::sync::broadcast;
use tokio::time::{self, Duration};
use tokio::signal;
use warp::filters::ws::Message;
use warp::{http::Response, Filter};
use futures_util::{StreamExt, SinkExt};
use std::path::PathBuf;
use tracing::{info, error};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::format::Writer;
use std::fmt;
use chrono::Local;

// 自定义时间格式，只显示时间
struct TimeOnly;

impl FormatTime for TimeOnly {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
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

    // 提示服务器启动
    info!("Starting server on http://0.0.0.0:8848");

    // 创建一个广播通道用于文件更新通知
    let (tx, _rx) = broadcast::channel::<String>(10);
    let tx = Arc::new(tx);

    // 共享的文件路径和状态
    let file_path = Arc::new(RwLock::new(None));
    let no_pdf_logged = Arc::new(RwLock::new(false));

    // 提供 HTML 页面
    let html_route = warp::path::end().map(|| {
        Response::builder()
            .header("content-type", "text/html")
            .body(INDEX_HTML)
    });

    // 提供 PDF 文件的静态文件服务
    let pdf_route = warp::path("pdf").and(warp::fs::dir("."));

    // 提供当前最新的 PDF URL
    let content_route = {
        let file_path = file_path.clone();
        warp::path("content").map(move || {
            let path = file_path.read().unwrap();
            let response = match &*path {
                Some(file_name) => warp::reply::json(&format!("/pdf/{}", file_name)),
                None => warp::reply::json(&"No PDF file found".to_string()),
            };
            response
        })
    };

    // WebSocket 路由用于实时更新
    let ws_route = {
        let tx = tx.clone();
        warp::path("ws")
            .and(warp::ws())
            .map(move |ws: warp::ws::Ws| {
                let tx = tx.clone();
                ws.on_upgrade(move |websocket| client_connection(websocket, tx.subscribe()))
            })
    };

    // 创建一个任务监控目录变化
    let file_path_arc = file_path.clone();
    let no_pdf_logged_arc = no_pdf_logged.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            match find_first_pdf_in_dir(".").await {
                Some(pdf_path) => {
                    let pdf_file_name = pdf_path.file_name().unwrap().to_string_lossy().to_string();
                    let mut path = file_path_arc.write().unwrap();
                    let mut no_pdf_logged = no_pdf_logged_arc.write().unwrap();
                    if *path != Some(pdf_file_name.clone()) {
                        *path = Some(pdf_file_name.clone());
                        let pdf_url = format!("/pdf/{}", pdf_file_name);
                        let _ = tx.send(pdf_url);
                    }
                    *no_pdf_logged = false;
                }
                None => {
                    let mut no_pdf_logged = no_pdf_logged_arc.write().unwrap();
                    if !*no_pdf_logged {
                        error!("No PDF file found in directory");
                        *no_pdf_logged = true;
                        let _ = tx.send("No PDF file found".to_string());
                    }
                }
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

// 查找当前目录中第一个按名称排序的 PDF 文件
async fn find_first_pdf_in_dir(dir: &str) -> Option<PathBuf> {
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return None,
    };

    let mut pdf_files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if let Some(ext) = path.extension() {
            if ext == "pdf" {
                pdf_files.push(path);
            }
        }
    }

    pdf_files.sort();
    pdf_files.first().cloned()
}

// 处理 WebSocket 客户端连接
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

// 内嵌 HTML 内容
const INDEX_HTML: &str = r#"
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>PDF Viewer</title>
</head>
<body>
    <div id="message"></div>
    <iframe id="pdf-frame" style="width: 100%; height: 100vh;" frameborder="0"></iframe>
    <script>
        const pdfFrame = document.getElementById("pdf-frame");
        const messageDiv = document.getElementById("message");

        async function fetchFileContent() {
            const response = await fetch('/content');
            const data = await response.json();
            displayPDF(data);
        }

        function setupWebSocket() {
            const ws = new WebSocket(`ws://${window.location.host}/ws`);
            ws.onmessage = (event) => {
                displayPDF(event.data);
            };
        }

        function displayPDF(url) {
            if (url === "No PDF file found") {
                messageDiv.textContent = url;
                pdfFrame.style.display = "none";
            } else {
                messageDiv.textContent = "";
                pdfFrame.style.display = "block";
                pdfFrame.src = url;
            }
        }

        fetchFileContent();
        setupWebSocket();
    </script>
</body>
</html>
"#;
