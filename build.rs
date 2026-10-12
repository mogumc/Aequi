//! 构建脚本。
//!
//! 只做一件事：保证前端产物目录存在。
//!
//! 面板**源码**在项目外的 `../dev-web`，但其构建**产物**输出到本项目的
//! `web/` 目录（见 dev-web/vite.config.js 里的 outDir）。后端只认自己项目内的
//! 这个目录，不跨目录去捞源码那边的中间产物。
//!
//! 产物可以不进版本库，但 `rust-embed` 需要目录真实存在才能展开宏——
//! 刚 clone 下来还没构建过前端时，这里建一个空目录，编译照常通过，
//! 运行时会显示「控制台尚未构建」的提示页，比编译失败好排查。

use std::path::Path;

/// 与 `src/web/mod.rs` 里 `#[folder = ...]` 保持一致。
const WEB_DIST: &str = "web";

fn main() {
    if !Path::new(WEB_DIST).exists() {
        std::fs::create_dir_all(WEB_DIST).expect("创建前端产物目录失败");
        println!(
            "cargo:warning={WEB_DIST} 不存在，已创建空目录；请到 Aequi-web 目录执行 npm install && npm run build 生成前端资源"
        );
    }

    // 前端产物变化后需要重新嵌入。
    println!("cargo:rerun-if-changed={WEB_DIST}");

    // CI 打包时注入发布版本号（如 v1.2.3），运行时由 `version()` 优先取用；
    // 没有该变量（本地构建）就不注入，运行时回退 Cargo.toml 的包版本。
    // rerun-if-env-changed 保证换 tag 重新打包时本 crate 会重编译。
    println!("cargo:rerun-if-env-changed=AEQUI_VERSION");
    if let Ok(version) = std::env::var("AEQUI_VERSION") {
        println!("cargo:rustc-env=AEQUI_VERSION={version}");
    }
}
