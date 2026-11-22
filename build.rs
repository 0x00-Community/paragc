fn main() {
    println!("cargo::rustc-check-cfg=cfg(crossheap)");
    if std::env::var_os("CARGO_FEATURE_CROSSHEAP").is_some() && std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux") {
        println!("cargo::rustc-cfg=crossheap");
    }
}
