//! 实机测试专用 Native Host：显式注入临时桥接目录，不占用用户当前浏览器连接。
use golutra_agent_tools::browser::chrome::transport;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let directory = PathBuf::from(
        std::env::var_os("GOLUTRA_AGENT_CHROME_TEST_CONNECTION_DIR")
            .expect("explicit test connection directory required"),
    );
    assert!(directory.is_absolute());
    let origin = std::env::args().nth(1).expect("extension origin required");
    transport::serve(&directory, &origin).await.unwrap();
}
