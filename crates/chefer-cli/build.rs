fn main() {
    // 編譯時間（UTC RFC3339）
    println!(
        "cargo:rustc-env=BUILD_TIME={}",
        chrono::Utc::now().to_rfc3339()
    );

    // target triple
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown-target".into());
    println!("cargo:rustc-env=BUILD_TARGET={target}");
}
