fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_dir_path = std::path::Path::new(&out_dir);
    // OUT_DIR = target/{profile}/build/holo_proxy-xxx/out
    // 往上 3 级 → target/{profile}/
    let target_dir = out_dir_path.ancestors().nth(3).unwrap();

    // ── 1. 复制 assets/ 目录 ──
    let src_assets = std::path::Path::new("assets");
    let dst_assets = target_dir.join("assets");
    if src_assets.exists() {
        let _ = std::fs::remove_dir_all(&dst_assets);
        copy_dir(src_assets, &dst_assets).ok();
        println!("cargo:warning=Assets → {}", dst_assets.display());
    }

    // ── 2. 复制 settings.json ──
    let src_settings = std::path::Path::new("settings.json");
    let dst_settings = target_dir.join("settings.json");
    if src_settings.exists() {
        std::fs::copy(src_settings, &dst_settings).ok();
        println!("cargo:warning=settings.json → {}", dst_settings.display());
    }
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
