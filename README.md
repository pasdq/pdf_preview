# PDF Viewer Server

This Rust project is a simple server that monitors a directory for PDF files and serves the latest PDF file to connected clients via a WebSocket connection. Clients can view the PDF in their browser in real-time as files are added or updated in the directory.

**Features**

- Real-time PDF Monitoring: Detects new or updated PDF files in the directory.
- WebSocket Updates: Sends notifications to clients when a new PDF is available.
- Static PDF Serving: Serves PDF files via a static route.
- HTML Interface: Provides a simple HTML interface to display the latest PDF.
