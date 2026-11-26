use std::{env, fs, path::PathBuf};

use tokio::process::Command;
use uuid::Uuid;

pub(crate) struct RetryTool {
    pub root: PathBuf,
}

impl RetryTool {
    pub async fn build() -> Self {
        let root = env::temp_dir().join(format!("slipstream-retry-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let tool = Self { root };
        let status = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg("--edition=2021")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/http_retry_tool.rs"
            ))
            .arg("-o")
            .arg(tool.executable())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        tool
    }

    pub fn executable(&self) -> PathBuf {
        self.root
            .join(format!("retry-tool{}", env::consts::EXE_SUFFIX))
    }

    pub fn attempts(&self, case: &str) -> String {
        fs::read_to_string(self.root.join(case)).unwrap()
    }
}

impl Drop for RetryTool {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
