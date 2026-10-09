//! 将同一构建的管理注册嵌入插件，避免运行时目录依赖
fn main() {
    println!("cargo:rerun-if-changed=../dist/plugin/management.json");
    println!("cargo:rerun-if-changed=../plugin.json");
    assert!(
        std::path::Path::new("../dist/plugin/management.json").exists(),
        "run npm run build before cargo build"
    );
}
